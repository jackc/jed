//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

pub(crate) use crate::api::Rows;
pub(crate) use crate::ast::{
    AlterSeqAction, AlterSequence, BinaryOp, ConflictAction, ConflictTarget, CreateIndex,
    CreateSequence, CreateTable, CreateType, Cte, CteBody, Delete, DropIndex, DropSequence,
    DropTable, DropType, Expr, GroupItem, Insert, InsertSource, InsertValue, JoinKind,
    JsonOnBehavior, JsonPredicateKind, JsonTable, JsonWrapper, JtColumn, Literal, OnConflict,
    OrderKey, Overriding, QueryExpr, RefAction, Select, SelectItems, SeqOptions, SetOp, SetOpKind,
    Statement, SubscriptSpec, TableRef, TypeFieldDef, TypeMod, UnaryOp, Update, WindowDef,
    WithExpr, WithQuery,
};
pub(crate) use crate::catalog::{
    CheckConstraint, ColField, ColType, Column, CompositeField, CompositeType, DefaultExpr,
    ExclusionConstraint, ExclusionElement, ExclusionOp, FkAction, ForeignKeyConstraint,
    IdentityKind, IndexDef, IndexKind, SeqDataType, SeqOwner, SequenceDef, Table, resolve_col_type,
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
mod eval;
mod exec_emit;
mod exec_scan;
mod execute;
mod explain_exec;
mod kernels;
mod plan_query;
mod planner;
mod srf;
pub(crate) use kernels::*;
mod store_encode;
pub(crate) use store_encode::*;
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
    /// a prepared statement's plan cache keys its committed-plan validity on it: a cached plan is
    /// reusable iff the read snapshot's `cat_gen` still equals the plan's (spec/design/api.md §2.4).
    /// NOT bumped by sequence `nextval` (a data write on the nextval path), only by sequence DDL — a
    /// SELECT plan binds no sequence.
    pub(crate) cat_gen: u64,
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

impl Snapshot {
    /// Look up a table definition by name (case-insensitive).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// The canonical name of every table in this snapshot, sorted ascending by lowercased name (the
    /// catalog's standing order — no map-iteration order may leak, CLAUDE.md §8). Secondary indexes
    /// are not tables and are excluded (api.md §6).
    pub fn table_names(&self) -> Vec<String> {
        let mut named: Vec<(&str, &str)> = self
            .tables
            .iter()
            .map(|(key, t)| (key.as_str(), t.name.as_str()))
            .collect();
        named.sort_by(|a, b| a.0.cmp(b.0));
        named
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect()
    }

    /// All tables in ascending lowercased-name order — a deterministic order with no map-iteration
    /// leak (CLAUDE.md §8); the jed_tables / jed_columns generation order
    /// (spec/design/introspection.md §5).
    pub(crate) fn tables_sorted(&self) -> Vec<&Table> {
        let mut keys: Vec<&String> = self.tables.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.tables[k]).collect()
    }

    /// Look up a composite type definition by name (case-insensitive).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.types.get(&name.to_ascii_lowercase())
    }

    /// Advance the catalog generation — called by every schema mutator (see `cat_gen`). A SELECT
    /// plan cached against a prior generation is thereby invalidated on the next execute.
    pub(crate) fn bump_cat_gen(&mut self) {
        self.cat_gen += 1;
    }

    /// Bind this snapshot's NEW stores to a per-domain `MemoryBlockStore` paging context (the temp seam
    /// — spec/design/temp-tables.md §6, attached-databases.md §6). Set on a host-attached in-memory
    /// database's committed root at attach time (shared.rs) so its tables/indexes ride the same pager +
    /// packed-leaf path as an in-memory database. NEVER serialized (an attachment snapshot never is).
    pub(crate) fn set_store_paging(&mut self, paging: std::sync::Arc<crate::paging::SharedPaging>) {
        self.store_paging = Some(paging);
    }

    /// Register a composite type (CREATE TYPE). Lower-cased name is the key. The caller has
    /// already resolved field types and checked for a duplicate.
    pub(crate) fn put_type(&mut self, ty: CompositeType) {
        self.bump_cat_gen();
        std::sync::Arc::make_mut(&mut self.types).insert(ty.name.to_ascii_lowercase(), ty);
    }

    /// Remove a composite type (DROP TYPE). The caller has checked there are no dependents.
    pub(crate) fn remove_type(&mut self, key: &str) {
        self.bump_cat_gen();
        std::sync::Arc::make_mut(&mut self.types).remove(key);
    }

    /// All composite types in ascending lowercased-name order — the on-disk emission order
    /// (spec/fileformat/format.md) and a deterministic order with no hash-iteration leak (§8).
    pub(crate) fn composite_types_sorted(&self) -> Vec<&CompositeType> {
        let mut keys: Vec<&String> = self.types.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.types[k]).collect()
    }

    /// Look up a sequence by name (case-insensitive).
    pub fn sequence(&self, name: &str) -> Option<&SequenceDef> {
        self.sequences.get(&name.to_ascii_lowercase())
    }

    /// Register a sequence (CREATE SEQUENCE). Lower-cased name is the key. The caller has already
    /// validated the option set and checked the relation namespace for a collision.
    pub(crate) fn put_sequence(&mut self, seq: SequenceDef) {
        std::sync::Arc::make_mut(&mut self.sequences).insert(seq.name.to_ascii_lowercase(), seq);
    }

    /// Remove a sequence (DROP SEQUENCE). The caller has checked it exists.
    pub(crate) fn remove_sequence(&mut self, key: &str) {
        std::sync::Arc::make_mut(&mut self.sequences).remove(key);
    }

    /// Resolve a collation name for USE — query resolution and key encoding (spec/design/collation.md
    /// §2/§9). The collations the database has resolved (a cache populated on open from the file's
    /// reference entries, carrying their version pin) first, then the engine-global **loaded** set
    /// (`db.LoadUnicodeData`, §4). `None` ⇒ neither has it (the resolver raises 42704). `C` is handled
    /// by the caller (built-in). This is the reference-only read path: a collation is never baked into
    /// the file — the file references it by name and the table comes from a loaded bundle.
    pub(crate) fn resolve_collation(&self, name: &str) -> Option<std::sync::Arc<Collation>> {
        self.collations
            .get(name)
            .cloned()
            .or_else(|| crate::collation::loaded_collation(name))
    }

    /// Record a collation resolved from a file reference entry on open (its file metadata + the
    /// vendored table), keyed by name, so later resolution preserves the file's version pin.
    pub(crate) fn put_collation(&mut self, coll: std::sync::Arc<Collation>) {
        std::sync::Arc::make_mut(&mut self.collations).insert(coll.name.clone(), coll);
    }

    /// The slice-2d version-skew verdict for a referenced collation (spec/design/collation.md §12):
    /// `Some((file_unicode, file_cldr, loaded_unicode, loaded_cldr))` if this database's keys were
    /// built under a different `(unicode, cldr)` than the loaded bundle provides — the object that
    /// uses it is read-only (`XX002` on write). `None` ⇒ `Full` (same version, or this collation has
    /// no catalog-local file pin so it is freshly the loaded version — an in-memory-only database).
    /// A pure comparison of the file pin already in the catalog (§5) vs the engine-global loaded set;
    /// `loaded_collation` is `Some` post-open (open refuses an absent reference), so a missing loaded
    /// table is not skew. The `Snapshot`-level wiring of `collation::version_skew`.
    pub(crate) fn collation_skew(&self, name: &str) -> Option<(String, String, String, String)> {
        let cat = self.collations.get(name)?;
        crate::collation::version_skew(name, &cat.unicode_version, &cat.cldr_version).map(
            |(lu, lc)| {
                (
                    cat.unicode_version.clone(),
                    cat.cldr_version.clone(),
                    lu,
                    lc,
                )
            },
        )
    }

    /// The collations the database **schema references** — every column's frozen collation plus the
    /// per-database default — resolved (catalog-local set, then the binary's vendored set) and sorted
    /// by exact name. Under the reference-only model (spec/design/collation.md §2/§5) these, not an
    /// imported set, are what earn a metadata entry on disk: a collation is recorded because the
    /// schema uses it, regardless of whether it was ever passed to a (now-removed) import call. `C`
    /// columns (`collation == None`) reference nothing. A referenced name this build does not vendor
    /// is a bug surfaced here (the precursor to the slice-2d open-time verdict).
    pub(crate) fn referenced_collations(&self) -> Result<Vec<std::sync::Arc<Collation>>> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for t in self.tables.values() {
            for col in &t.columns {
                if let Some(n) = &col.collation {
                    names.insert(n.clone());
                }
            }
        }
        if let Some(d) = &self.default_collation {
            names.insert(d.clone());
        }
        names
            .into_iter()
            .map(|name| {
                self.resolve_collation(&name).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedObject,
                        format!(
                            "collation \"{name}\" referenced by the schema is not provided by a loaded bundle"
                        ),
                    )
                })
            })
            .collect()
    }

    /// The REINDEX / COLLATION UPGRADE migration (spec/design/collation.md §12): rebuild every
    /// collated key stored under a version-**skewed** collation against the **loaded** table and
    /// advance that collation's pin to the loaded version — clearing the skew so the affected tables
    /// are read-write again and their collated indexes regain pushdown (a `Full` index,
    /// encoding.md §2.12). Returns the number of collations re-pinned (`0` ⇒ nothing was skewed, a
    /// no-op).
    ///
    /// **Whole-database, per-collation pin.** The pin is **one entry per collation NAME** (§5), so a
    /// collation's pin may advance only once **every** key under it (across all tables) is rebuilt —
    /// else a not-yet-rebuilt table would falsely read as `Full` (silent corruption). This rebuilds
    /// all skewed collations' keys and re-pins them together; the caller swaps the result in atomically
    /// (one root publish). Adoption is **explicit** — never automatic on open (§12).
    ///
    /// `resolve_collation` already yields the loaded table data (the file entry carries the file
    /// *pin* but the loaded singles/contractions — `decode_collation_entry`), so re-encoding a key
    /// produces **loaded-version** sort keys; the re-pin only realigns the version label.
    pub(crate) fn upgrade_collations(&mut self, page_size: u32) -> Result<usize> {
        // 1. The skewed set: referenced collations whose file pin differs from the loaded version.
        let skewed: std::collections::BTreeSet<String> = self
            .referenced_collations()?
            .into_iter()
            .filter(|c| self.collation_skew(&c.name).is_some())
            .map(|c| c.name.clone())
            .collect();
        if skewed.is_empty() {
            return Ok(0);
        }
        let is_skewed = |coll: &Option<String>| coll.as_ref().is_some_and(|n| skewed.contains(n));

        // 2. Rebuild each affected table's collated trees under the loaded collations. Sorted table
        // order so no HashMap iteration order leaks (CLAUDE.md §8); the per-table rebuilds are
        // independent and the re-pin is order-free, so the result is order-invariant regardless, but
        // the sort keeps it manifestly so.
        let mut table_keys: Vec<String> = self.tables.keys().cloned().collect();
        table_keys.sort();
        for key in table_keys {
            let table = self
                .tables
                .get(&key)
                .expect("table key from this map")
                .clone();
            // A collated PK key is re-encoded ⇒ every row's storage key moves ⇒ a full table rewrite,
            // and since an index entry carries the storage key as its suffix (indexes.md §3) every
            // index of the table must be rebuilt too. Otherwise only the indexes whose own key
            // columns use a skewed collation are rebuilt (the table store keeps its keys). A skewed
            // collation used ONLY by a non-key column needs no rebuild — values are version-independent.
            let pk_skewed = table
                .pk
                .iter()
                .any(|&i| is_skewed(&table.columns[i].collation));
            let indexes: Vec<IndexDef> = table
                .indexes
                .iter()
                .filter(|idx| {
                    pk_skewed
                        || idx
                            .columns
                            .iter()
                            .any(|&c| is_skewed(&table.columns[c].collation))
                })
                .cloned()
                .collect();
            if !pk_skewed && indexes.is_empty() {
                continue;
            }
            // The per-column collations resolved against the LOADED set (the table data is loaded;
            // only the pin label is the file version) — what re-encodes each key to the loaded version.
            let colls: Vec<Option<std::sync::Arc<Collation>>> = table
                .columns
                .iter()
                .map(|c| c.collation.as_ref().and_then(|n| self.resolve_collation(n)))
                .collect();
            let pk: Vec<(usize, Type)> = table
                .pk
                .iter()
                .map(|&i| (i, table.columns[i].ty.clone()))
                .collect();
            // Read every (storage key, row) pair, fully materialized (a spilled non-key value must
            // survive a table rewrite). A collated key column never spills (§2.12 narrowing b), so
            // the keys are always inline.
            let mut entries: Vec<(Vec<u8>, Row)> = {
                let store = self.store(&key);
                let mut es = store.iter_entries()?;
                for (_, row) in &mut es {
                    store.resolve_all(row)?;
                }
                es
            };
            // The NEW storage key per row: re-encoded under the loaded collation if the PK moved,
            // else the existing key (unchanged — includes a synthetic rowid table, which has no PK).
            for (k, row) in &mut entries {
                if pk_skewed {
                    *k = encode_pk_key(&pk, &colls, row)?;
                }
            }
            // 2a. Re-key the table store (fresh empty store via `put_table`, then re-insert).
            if pk_skewed {
                self.put_table(table.clone(), page_size);
                for (k, row) in &entries {
                    self.store_mut(&key).insert(k.clone(), row.clone())?;
                }
            }
            // 2b. Rebuild each affected index store from the (re-keyed) rows.
            let cap = crate::format::page_payload(page_size);
            for def in &indexes {
                let mut ekeys: Vec<Vec<u8>> = Vec::new();
                for (k, row) in &entries {
                    ekeys.extend(index_entry_keys(&table.columns, &colls, def, k, row)?);
                }
                ekeys.sort_unstable();
                let mut fresh = TableStore::new(cap, Vec::new());
                for ek in ekeys {
                    fresh.insert(ek, Vec::new())?;
                }
                self.put_index_store(def.name.to_ascii_lowercase(), fresh);
            }
        }

        // 3. Advance each skewed collation's pin to the loaded version (realign the label to the
        // table data already in use). `referenced_collations` then resolves the advanced pin and the
        // commit persists it; `collation_skew` now returns `None` (Full) for each.
        for name in &skewed {
            if let Some(loaded) = crate::collation::loaded_collation(name) {
                self.put_collation(loaded);
            }
        }
        Ok(skewed.len())
    }

    /// The per-database default collation name, or `None` for `C` (spec/design/collation.md §1).
    pub(crate) fn default_collation(&self) -> Option<&str> {
        self.default_collation.as_deref()
    }

    /// Set the per-database default collation (`db.set_default_collation`). `None` ⇒ `C`. The caller
    /// has validated the name is loaded.
    pub(crate) fn set_default_collation(&mut self, name: Option<String>) {
        self.default_collation = name;
    }

    /// All sequences in ascending lowercased-name order — the on-disk emission order
    /// (spec/fileformat/format.md) and a deterministic order with no hash-iteration leak (§8).
    pub(crate) fn sequences_sorted(&self) -> Vec<&SequenceDef> {
        let mut keys: Vec<&String> = self.sequences.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.sequences[k]).collect()
    }

    /// The lowercased keys of every sequence **owned by** the table `name` (case-insensitive) — the
    /// `serial`-created sequences `DROP TABLE` must auto-drop (spec/design/sequences.md §12). Returned
    /// in ascending key order so the auto-drop is deterministic (no hash-iteration leak, §8).
    pub(crate) fn sequences_owned_by(&self, name: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .sequences
            .iter()
            .filter(|(_, s)| {
                s.owned_by
                    .as_ref()
                    .is_some_and(|o| o.table.eq_ignore_ascii_case(name))
            })
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// Whether any table column or composite-type field still references the composite type
    /// `name` (case-insensitive) — the `DROP TYPE ... RESTRICT` dependency check (2BP01). Returns
    /// the first dependent's description for the error detail, or `None` if there are no dependents.
    pub(crate) fn composite_dependent(&self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        // `composite_ref` looks through one array level, so an `addr[]` column / field counts as a
        // dependent of `addr` exactly as a bare `addr` one does (spec/design/array.md §12).
        for t in self.tables.values() {
            for c in &t.columns {
                if c.ty
                    .composite_ref()
                    .is_some_and(|r| r.name.eq_ignore_ascii_case(&key))
                {
                    return Some(format!("column {} of table {}", c.name, t.name));
                }
            }
        }
        for ct in self.types.values() {
            for f in &ct.fields {
                if f.ty
                    .composite_ref()
                    .is_some_and(|r| r.name.eq_ignore_ascii_case(&key))
                {
                    return Some(format!("field {} of type {}", f.name, ct.name));
                }
            }
        }
        None
    }

    /// Every FK on a table **not** in `dropping` (a set of lowercased table keys) that references
    /// a table that **is** in `dropping` — the dependency scan for a multi-table `DROP TABLE`
    /// (spec/design/grammar.md §13, constraints.md §6.10). A dependent whose referencing table is
    /// itself being dropped does not count (the drop-set exclusion), so a FK between two tables
    /// both named in the same statement never blocks. Referencing tables are scanned in ascending
    /// lowercased key order (each table's `foreign_keys` is already name-ordered) for determinism
    /// (§8). `RESTRICT` raises 2BP01 on the first entry; `CASCADE` removes every entry's FK.
    pub(crate) fn foreign_key_dependents_excluding(
        &self,
        dropping: &BTreeSet<String>,
    ) -> Vec<FkDependent> {
        let mut out = Vec::new();
        let mut tkeys: Vec<&String> = self.tables.keys().collect();
        tkeys.sort();
        for tk in tkeys {
            if dropping.contains(tk) {
                continue; // the referencing table is itself being dropped — no dependency
            }
            let t = &self.tables[tk];
            for fk in &t.foreign_keys {
                let ref_key = fk.ref_table.to_ascii_lowercase();
                if dropping.contains(&ref_key) {
                    let dropped_name = self
                        .tables
                        .get(&ref_key)
                        .map_or_else(|| fk.ref_table.clone(), |d| d.name.clone());
                    out.push(FkDependent {
                        ref_table_key: tk.clone(),
                        fk_name: fk.name.clone(),
                        ref_table_name: t.name.clone(),
                        dropped_name,
                    });
                }
            }
        }
        out
    }

    /// Remove the named FK constraint from `table_key` in place, preserving the table's store and
    /// rows — `DROP TABLE … CASCADE`'s removal of a dependent FK on a table that *survives* the
    /// drop (spec/design/grammar.md §13). Only the catalog `foreign_keys` list changes; an FK
    /// owns no B-tree (constraints.md §6), so there is nothing else to remove.
    pub(crate) fn remove_foreign_key(&mut self, table_key: &str, fk_name: &str) {
        if let Some(table) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            self.cat_gen += 1;
            table
                .foreign_keys
                .retain(|fk| !fk.name.eq_ignore_ascii_case(fk_name));
        }
    }

    /// Validate the loaded composite-type catalog (the on-disk two-pass load —
    /// spec/design/composite.md §3): every composite a field references must exist, the reference
    /// graph must be acyclic, and no type may nest deeper than [`MAX_COMPOSITE_DEPTH`]. A dangling,
    /// cyclic, or over-deep reference is a malformed file (`XX001`). Called once after the whole
    /// catalog is read, and **before** any store is built — so the subsequent `resolve_col_type`
    /// walks (and every later value-codec/comparator walk) recurse over a depth-bounded catalog and
    /// stay stack-safe (CLAUDE.md §13; cost.md §7b).
    pub(crate) fn validate_composite_types(&self) -> Result<()> {
        // Existence: every composite a field references (directly, or as an array element —
        // `composite_ref` looks through one array level) names a registered type.
        for ct in self.types.values() {
            for f in &ct.fields {
                if let Some(r) = f.ty.composite_ref() {
                    if self.composite_type(&r.name).is_none() {
                        return Err(EngineError::new(
                            SqlState::DataCorrupted,
                            format!(
                                "composite type {} references unknown type {}",
                                ct.name, r.name
                            ),
                        ));
                    }
                }
            }
        }
        // One DFS over the type → referenced-types graph that enforces BOTH acyclicity and the
        // nesting-depth bound (color: 0 unvisited, 1 on-stack, 2 done; `cache` memoizes each done
        // type's absolute nesting depth). Two guards make it stack-safe AND sound regardless of
        // visitation order: `levels_above >= MAX` bounds the native recursion on a fresh descent,
        // and the post-compute `depth > MAX` check catches an over-deep type reached via a memoized
        // (color-2) shortcut — which the descent guard alone would miss when the catalog is colored
        // bottom-up. Existence ran first, so every referenced type is present.
        fn visit(
            snap: &Snapshot,
            key: &str,
            levels_above: usize,
            color: &mut HashMap<String, u8>,
            cache: &mut HashMap<String, usize>,
        ) -> Result<usize> {
            if levels_above >= MAX_COMPOSITE_DEPTH {
                return Err(EngineError::new(
                    SqlState::DataCorrupted,
                    format!(
                        "composite type nesting exceeds the maximum depth of {MAX_COMPOSITE_DEPTH}"
                    ),
                ));
            }
            match color.get(key).copied().unwrap_or(0) {
                1 => {
                    return Err(EngineError::new(
                        SqlState::DataCorrupted,
                        format!("composite type definition cycle through {key}"),
                    ));
                }
                2 => return Ok(*cache.get(key).unwrap_or(&1)),
                _ => {}
            }
            color.insert(key.to_string(), 1);
            let mut child = 0;
            if let Some(ct) = snap.types.get(key) {
                for f in &ct.fields {
                    if let Some(r) = f.ty.composite_ref() {
                        let ck = r.name.to_ascii_lowercase();
                        child = child.max(visit(snap, &ck, levels_above + 1, color, cache)?);
                    }
                }
            }
            let depth = 1 + child;
            if depth > MAX_COMPOSITE_DEPTH {
                return Err(EngineError::new(
                    SqlState::DataCorrupted,
                    format!(
                        "composite type nesting exceeds the maximum depth of {MAX_COMPOSITE_DEPTH}"
                    ),
                ));
            }
            color.insert(key.to_string(), 2);
            cache.insert(key.to_string(), depth);
            Ok(depth)
        }
        let mut color: HashMap<String, u8> = HashMap::new();
        let mut cache: HashMap<String, usize> = HashMap::new();
        let keys: Vec<String> = self.types.keys().cloned().collect();
        for k in keys {
            if color.get(&k).copied().unwrap_or(0) == 0 {
                visit(self, &k, 0, &mut color, &mut cache)?;
            }
        }
        Ok(())
    }

    /// The composite-type nesting depth of `ty` against this snapshot's type catalog, memoized in
    /// `cache` (lowercased name → depth): a scalar is 0, `T[]` is `depth(T)` (array levels are not
    /// composite levels — `composite_ref` looks through one array level the same way), and a
    /// composite is `1 + max(field depths)` (an empty composite is 1). The `CREATE TYPE` gate uses
    /// this against the *existing* catalog, every type of which already satisfies depth ≤
    /// [`MAX_COMPOSITE_DEPTH`] (the load + create invariant), so the recursion is bounded by the
    /// limit; memoization keeps a diamond-shaped reference graph linear (spec/design/cost.md §7b).
    pub(crate) fn composite_type_depth(
        &self,
        ty: &Type,
        cache: &mut HashMap<String, usize>,
    ) -> usize {
        let r = match ty.composite_ref() {
            Some(r) => r,
            None => return 0, // a scalar (or a scalar array) adds no composite level
        };
        let key = r.name.to_ascii_lowercase();
        if let Some(&d) = cache.get(&key) {
            return d;
        }
        let depth = match self.types.get(&key) {
            Some(def) => {
                1 + def
                    .fields
                    .iter()
                    .map(|f| self.composite_type_depth(&f.ty, cache))
                    .max()
                    .unwrap_or(0)
            }
            None => 1,
        };
        cache.insert(key, depth);
        depth
    }

    /// The store for a table (panics if absent — callers resolve the table first).
    pub(crate) fn store(&self, name: &str) -> &TableStore {
        self.stores
            .get(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// The store for a table, mutable (panics if absent).
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        std::sync::Arc::make_mut(&mut self.stores)
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table is absent. A
    /// test/debug convenience (the SELECT path scans through `iter_in_key_order` directly, propagating
    /// I/O errors); every value is fully materialized — the helper's callers compare whole rows, so
    /// no unfetched reference may escape (large-values.md §14). The fault-`Result` is unwrapped here.
    pub(crate) fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.stores.get(&name.to_ascii_lowercase()).map(|s| {
            let mut rows = s.iter_in_key_order().expect("test-helper read failed");
            for row in &mut rows {
                s.resolve_all(row).expect("test-helper resolve failed");
            }
            rows
        })
    }

    /// Register a new table and its (empty) store. Lower-cased name is the key. The store carries
    /// the page payload `cap` (= `page_size − 16`) and the column types so the page-backed B-tree
    /// can weigh records for its size-driven split (spec/fileformat/format.md).
    pub(crate) fn put_table(&mut self, table: Table, page_size: u32) {
        // Resolve each column's `ColType` against the (already-registered) composite-type catalog
        // — the codec/coercion tree the store keeps so neither re-walks the type catalog per row
        // (spec/design/composite.md §4). Composite types are registered before any table (the
        // types-first catalog order / `CREATE TYPE`-before-`CREATE TABLE` rule), so the lookup
        // inside `resolve_col_type` always resolves.
        let col_types: Vec<ColType> = table
            .columns
            .iter()
            .map(|c| resolve_col_type(&c.ty, &self.types))
            .collect();
        self.put_table_resolved(table, col_types, page_size);
    }

    /// Register a table whose column `ColType`s are **already resolved** — used when staging a TEMP
    /// table (spec/design/temp-tables.md §8): a temp table's composite columns must resolve against
    /// the MAIN snapshot's type catalog (composites are never temp — `CREATE TYPE` is persistent),
    /// not this (temp) snapshot's empty `types` map. The resolved [`ColType`] tree is fully
    /// self-contained (spec/design/composite.md §4), so the store needs nothing from the catalog
    /// thereafter. The plain [`put_table`](Snapshot::put_table) resolves against `self.types` and
    /// delegates here.
    pub(crate) fn put_table_resolved(
        &mut self,
        table: Table,
        col_types: Vec<ColType>,
        page_size: u32,
    ) {
        self.bump_cat_gen();
        let key = table.name.to_ascii_lowercase();
        let cap = crate::format::page_payload(page_size);
        let mut st = TableStore::new(cap, col_types);
        // Bind the domain's pager (`Snapshot::store_paging`) so the new store demand-pages like a
        // loaded one: its committed leaves demote at each commit (`demote_clean_leaves`) and fault
        // back through the pool, instead of staying fully-resident decoded for the handle's lifetime.
        // `None` only on a bare scratch engine that never persists.
        if let Some(paging) = &self.store_paging {
            st.attach_paging(paging.clone());
        }
        std::sync::Arc::make_mut(&mut self.stores).insert(key.clone(), st);
        std::sync::Arc::make_mut(&mut self.tables).insert(key, table);
    }

    /// Remove a table's definition, its store, and its indexes' stores (DROP TABLE — the
    /// indexes have no independent life, spec/design/indexes.md §2).
    fn remove_table(&mut self, key: &str) {
        self.bump_cat_gen();
        if let Some(t) = self.tables.get(key) {
            // Disjoint field borrows: `t` reads `self.tables` while we mutate `self.index_stores`.
            let index_stores = std::sync::Arc::make_mut(&mut self.index_stores);
            for idx in &t.indexes {
                index_stores.remove(&idx.name.to_ascii_lowercase());
            }
        }
        std::sync::Arc::make_mut(&mut self.tables).remove(key);
        std::sync::Arc::make_mut(&mut self.stores).remove(key);
    }

    /// The store of a secondary index (panics if absent — callers resolve the index first).
    pub(crate) fn index_store(&self, name_key: &str) -> &TableStore {
        self.index_stores
            .get(name_key)
            .expect("store exists for a resolved index")
    }

    /// The store of a secondary index, mutable (panics if absent).
    pub(crate) fn index_store_mut(&mut self, name_key: &str) -> &mut TableStore {
        std::sync::Arc::make_mut(&mut self.index_stores)
            .get_mut(name_key)
            .expect("store exists for a resolved index")
    }

    /// Whether this snapshot holds a store for the named index (lowercased key). Used to route
    /// index access to the session temp snapshot vs the main snapshot (temp-tables.md §2).
    pub(crate) fn has_index_store(&self, name_key: &str) -> bool {
        self.index_stores.contains_key(name_key)
    }

    /// Total on-disk record bytes of every table store + index store in this snapshot — the temp
    /// budget's deterministic footprint measure (spec/design/temp-tables.md §7), summed over the
    /// session temp snapshot. Iteration order does not matter (it is a sum).
    pub(crate) fn storage_bytes(&self) -> u64 {
        let tables: u64 = self.stores.values().map(|s| s.stored_bytes()).sum();
        let indexes: u64 = self.index_stores.values().map(|s| s.stored_bytes()).sum();
        tables + indexes
    }

    /// Register a new (empty) secondary index on `table_key`: insert its definition into the
    /// table's `indexes` in ascending lowercased-name order (the catalog/planner order —
    /// spec/design/indexes.md §6) and create its zero-column store.
    pub(crate) fn put_index(&mut self, table_key: &str, def: IndexDef, page_size: u32) {
        self.bump_cat_gen();
        let name_key = def.name.to_ascii_lowercase();
        let cap = crate::format::page_payload(page_size);
        let mut fresh = TableStore::new(cap, Vec::new());
        if let Some(paging) = &self.store_paging {
            // Bind the domain pager, like put_table_resolved / put_index_store.
            fresh.attach_paging(paging.clone());
        }
        std::sync::Arc::make_mut(&mut self.index_stores).insert(name_key.clone(), fresh);
        let table = std::sync::Arc::make_mut(&mut self.tables)
            .get_mut(table_key)
            .expect("table exists");
        let pos = table
            .indexes
            .iter()
            .position(|i| i.name.to_ascii_lowercase() > name_key)
            .unwrap_or(table.indexes.len());
        table.indexes.insert(pos, def);
    }

    /// Replace a table column's expression default **in place**, leaving the table's rows and store
    /// untouched — used by `ALTER SEQUENCE … RENAME` of an owned sequence to rewrite the owning
    /// column's `nextval` default (spec/design/sequences.md §15.3). `put_table` cannot be used here:
    /// it rebuilds a fresh empty store. A no-op if the table or column ordinal is absent.
    pub(crate) fn set_column_default_expr(
        &mut self,
        table_key: &str,
        column: usize,
        default_expr: DefaultExpr,
    ) {
        if let Some(table) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            if let Some(col) = table.columns.get_mut(column) {
                col.default_expr = Some(default_expr);
                self.cat_gen += 1;
            }
        }
    }

    /// Register a loaded index store under its (lowercased) name — the file loader's hook
    /// (format.rs): the owning table's `indexes` list came from its catalog entry, so only
    /// the store is registered here.
    pub(crate) fn put_index_store(&mut self, name_key: String, mut store: TableStore) {
        // An index store created in-session binds the domain's pager like a table store
        // (put_table_resolved) so it joins the post-commit residency flip; a store loaded from a
        // file already attached it.
        if let Some(paging) = &self.store_paging {
            if !store.is_file_backed() {
                store.attach_paging(paging.clone());
            }
        }
        std::sync::Arc::make_mut(&mut self.index_stores).insert(name_key, store);
    }

    /// Iterate every table data store — the store-page reachability walk (format.rs `reachable_pages`,
    /// the within-session compaction basis) reads each store's tree root + column types.
    pub(crate) fn stores_iter(&self) -> impl Iterator<Item = &TableStore> {
        self.stores.values()
    }

    /// Iterate every secondary/unique index store (empty-payload trees, never spillable).
    pub(crate) fn index_stores_iter(&self) -> impl Iterator<Item = &TableStore> {
        self.index_stores.values()
    }

    /// The resident GiST R-tree of the named index (lowercased key), or `None` if the index is not
    /// GiST / not present (spec/design/gist.md §4.1). The planner descends it for a `&&`/`@>` bound.
    pub(crate) fn gist_tree(
        &self,
        name_key: &str,
    ) -> Option<&std::sync::Arc<crate::gist::GistTree>> {
        self.gist_trees.get(name_key)
    }

    /// Rebuild **every** GiST index's resident R-tree from its leaf-key store (spec/design/gist.md
    /// §3/§4.1). Called after any statement that may have changed a GiST index's leaf set (the
    /// mutating-statement hook), so the working snapshot always carries a fresh tree a subsequent
    /// read descends — and after publish, the committed snapshot does too. Each tree is built in
    /// **canonical** order (`build_from_leaf_keys`: `range_total_cmp`, ties by storage key), making
    /// it a pure function of the leaf SET — content-deterministic, cross-core identical, and
    /// identical to the on-disk persisted R-tree. Trees whose index has been dropped are removed.
    /// A whole-tree rewrite, the §4.1(b) commit-rewrite narrowing extended to in-memory writes; the
    /// O(rows)-per-mutation cost is unmetered structure maintenance on the (trusted) write path —
    /// the untrusted surface is SELECT-only and never triggers it (gist.md §9, CLAUDE.md §13).
    pub(crate) fn rebuild_gist_trees(&mut self) -> Result<()> {
        // Collect (index name key, opclass) for every GiST index, dropping the borrow on
        // `self.tables` before mutating `self.gist_trees`.
        let mut specs: Vec<(String, Vec<crate::gist::GistOpclass>)> = Vec::new();
        for table in self.tables.values() {
            for idx in &table.indexes {
                if idx.kind != IndexKind::Gist {
                    continue;
                }
                // One opclass per indexed column (gist.md §7): a single-column GX1/GX2 index has
                // one; an EXCLUDE backing index has one per `WITH` column.
                let ops: Vec<crate::gist::GistOpclass> = idx
                    .columns
                    .iter()
                    .map(|&ci| crate::gist::opclass_for(&table.columns[ci].ty))
                    .collect();
                specs.push((idx.name.to_ascii_lowercase(), ops));
            }
        }
        let live: std::collections::HashSet<&str> = specs.iter().map(|(k, _)| k.as_str()).collect();
        // Disjoint field borrows: hold the mutable `gist_trees` while reading `self.index_stores`.
        let gist_trees = std::sync::Arc::make_mut(&mut self.gist_trees);
        gist_trees.retain(|k, _| live.contains(k.as_str()));
        for (name_key, ops) in &specs {
            let keys: Vec<Vec<u8>> = match self.index_stores.get(name_key) {
                Some(store) => store.iter_entries()?.into_iter().map(|(k, _)| k).collect(),
                None => Vec::new(),
            };
            let tree = crate::gist::build_from_leaf_keys(ops, keys.iter().map(|k| k.as_slice()))?;
            gist_trees.insert(name_key.clone(), std::sync::Arc::new(tree));
        }
        Ok(())
    }

    /// Remove one secondary index (DROP INDEX): its definition from the owning table and
    /// its store.
    fn remove_index(&mut self, table_key: &str, name_key: &str) {
        self.bump_cat_gen();
        if let Some(t) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            t.indexes
                .retain(|i| i.name.to_ascii_lowercase() != name_key);
        }
        std::sync::Arc::make_mut(&mut self.index_stores).remove(name_key);
    }

    /// Find the table owning the named index (case-insensitive): `(table_key, &IndexDef)`.
    fn find_index(&self, name: &str) -> Option<(&str, &IndexDef)> {
        let key = name.to_ascii_lowercase();
        self.tables.iter().find_map(|(tk, t)| {
            t.indexes
                .iter()
                .find(|i| i.name.to_ascii_lowercase() == key)
                .map(|i| (tk.as_str(), i))
        })
    }

    /// Every table with its store, as `(lowercased key, table, store)` tuples, for the on-disk
    /// serializer (spec/fileformat/format.md). The serializer sorts by the lowercased key so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    /// Demote every store's clean, persisted resident leaves to `OnDisk` references — the
    /// post-commit residency flip over the whole snapshot (bplus-reshape.md B4), run after a
    /// successful persist so the published committed tree is the skeletal `interiors + OnDisk
    /// leaves` shape on every host. Table stores and btree/GIN index stores flip; a GiST leaf-key
    /// store's nodes are never persisted (its on-disk form is the R-tree), so it no-ops naturally.
    pub(crate) fn demote_clean_leaves(&mut self) {
        for store in std::sync::Arc::make_mut(&mut self.stores).values_mut() {
            store.demote_clean_leaves();
        }
        for store in std::sync::Arc::make_mut(&mut self.index_stores).values_mut() {
            store.demote_clean_leaves();
        }
    }

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

impl Engine {
    pub fn new() -> Self {
        Engine::with_page_size(DEFAULT_PAGE_SIZE)
    }

    /// An in-memory handle that serializes at `page_size`. The page-backed B-tree's fan-out tracks
    /// the page size (spec/fileformat/format.md), so the in-memory tree must be built at the size it
    /// will serialize to — this builds fixtures / tests a non-default page size; a normal in-memory
    /// database uses [`Engine::new`] (the default page size).
    pub fn with_page_size(page_size: u32) -> Self {
        Engine {
            committed: Snapshot::default(),
            path: None,
            page_size,
            page_count: 0,
            free_pages: Vec::new(),
            live_at_compaction: 0,
            paging: None,
            read_only: false,
            session: SessionState::new(),
            temp_storage: None,
            open_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            core: None,
            attached_committed: HashMap::new(),
        }
    }

    /// Build an in-memory handle whose committed state **is** `snap` (no file backing). The
    /// thread-safe shared layer ([`crate::shared`]) uses this to run the unchanged executor against
    /// a snapshot it has pinned from the shared committed cell: a read handle keeps one of these
    /// with no open transaction (reads hit `committed` = the pinned snapshot); a write handle keeps
    /// one with an open READ WRITE block and publishes its working set back to the shared cell.
    pub(crate) fn from_snapshot(snap: Snapshot) -> Self {
        Engine {
            committed: snap,
            path: None,
            page_size: DEFAULT_PAGE_SIZE,
            page_count: 0,
            free_pages: Vec::new(),
            live_at_compaction: 0,
            paging: None,
            read_only: false,
            session: SessionState::new(),
            temp_storage: None,
            open_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            core: None,
            attached_committed: HashMap::new(),
        }
    }

    /// The snapshot a read sees: the **read pin** if one is set (a data-modifying `WITH` statement
    /// pins the pre-statement snapshot so every sub-statement reads it — writable-cte.md §2), else
    /// the open transaction's `working` (read-your-writes for a writable tx; the pinned snapshot for
    /// a read-only tx), else the committed snapshot.
    fn read_snap(&self) -> &Snapshot {
        if let Some(pin) = &self.session.read_pin {
            return pin;
        }
        match &self.session.tx {
            Some(tx) => &tx.working,
            None => &self.committed,
        }
    }

    /// Resolve each column's frozen collation (`Column::collation`, the name) to its baked table,
    /// indexed by column ordinal — `None` for a `C` / non-text column (the fast path). The key
    /// encoders (§2.12) consult `colls[ci]` to pick a text column's key form. Returns owned `Arc`
    /// clones (cheap), so the result outlives the snapshot borrow and composes with the mutable
    /// store borrow that phase-2 writes hold (collations are immutable within a statement).
    fn column_collations(&self, columns: &[Column]) -> Vec<Option<std::sync::Arc<Collation>>> {
        let snap = self.read_snap();
        columns
            .iter()
            .map(|c| c.collation.as_ref().and_then(|n| snap.resolve_collation(n)))
            .collect()
    }

    /// Refuse a WRITE that would maintain a collated B-tree under a **version-skewed** collation
    /// (the slice-2d verdict, spec/design/collation.md §12/§14): if any of `columns` carries a
    /// collation the file pinned to a different `(unicode, cldr)` than the loaded bundle provides,
    /// inserting/updating/deleting/index-building would mix two orderings in one tree and corrupt it,
    /// so the whole table is **read-only** until a REINDEX migration (deferred) rebuilds + re-pins it.
    /// `XX002`, naming the collation + both versions. Reads never call this — they recompute against
    /// the loaded table (the heap-scan fallback, compatibility.md §8). Per-table granularity: one
    /// skewed column collation makes the table read-only (finer per-index gating is a follow-on).
    fn ensure_collations_writable(&self, columns: &[Column]) -> Result<()> {
        let snap = self.read_snap();
        for c in columns {
            if let Some(name) = &c.collation
                && let Some((fu, fc, lu, lc)) = snap.collation_skew(name)
            {
                return Err(EngineError::new(
                    SqlState::CollationVersionMismatch,
                    format!(
                        "collation \"{name}\" version mismatch: this database's keys were built under \
                         {fu}/{fc} but the loaded bundle is {lu}/{lc}; tables using it are read-only \
                         until a REINDEX migration rebuilds them"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refresh the main working snapshot's resident GiST trees **iff** the current statement mutated
    /// the main image (spec/design/gist.md §3/§4.1). Run after a statement so a subsequent read —
    /// within the same transaction or, after publish, against the committed snapshot — descends a
    /// fresh, canonically-rebuilt tree. Gated on `main_dirty` (set by the statement's own
    /// `working_mut` writes): a read or a temp-only write leaves it unset, so this is a no-op and
    /// never forces a spurious main-image persist (the temp-no-file-write invariant, temp-tables.md
    /// §2). Trees on temp snapshots are out of scope this slice (GiST on a temp table is
    /// `0A000`, gist.md §11), so only the main working snapshot is refreshed.
    fn rebuild_main_gist_trees_if_dirty(&mut self) -> Result<()> {
        if let Some(tx) = self.session.tx.as_mut()
            && tx.main_dirty
        {
            tx.working.rebuild_gist_trees()?;
        }
        Ok(())
    }

    /// The working snapshot a write mutates — the open transaction's `working`. A write only ever
    /// runs with a transaction open (autocommit opens one implicitly), so this never panics in a
    /// correct flow.
    fn working_mut(&mut self) -> &mut Snapshot {
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a write statement runs within a transaction");
        // Mark the main image dirty so the commit knows to persist it; a temp-only transaction never
        // reaches here and so makes zero file writes (spec/design/temp-tables.md §2).
        tx.main_dirty = true;
        &mut tx.working
    }

    /// The session's temp-table snapshot for READS (spec/design/temp-tables.md §2): the open
    /// transaction's `temp_working`, else the session's committed temp state. The temp analogue of
    /// [`read_snap`](Engine::read_snap) (it does not consult `read_pin` — a writable-CTE pins only
    /// the main snapshot).
    fn temp_read_snap(&self) -> &Snapshot {
        match &self.session.tx {
            Some(tx) => &tx.temp_working,
            None => &self.session.temp_committed,
        }
    }

    /// The session's temp-table snapshot for WRITES — the open transaction's `temp_working`. A temp
    /// write opens an (implicit autocommit) transaction just like a main write, so this is present;
    /// it also flags `temp_dirty` so the commit can skip persisting the (unchanged) main image.
    fn temp_working_mut(&mut self) -> &mut Snapshot {
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a temp write statement runs within a transaction");
        tx.temp_dirty = true;
        &mut tx.temp_working
    }

    /// Whether `name` resolves to a SESSION-LOCAL temporary table in the visible temp snapshot
    /// (spec/design/temp-tables.md §3). Preclude-overlaps guarantees a name is temp XOR persistent,
    /// so this is the routing predicate the table/store funnels use.
    fn is_temp_table(&self, name: &str) -> bool {
        self.temp_read_snap().table(name).is_some()
    }

    /// Validate an optional database qualifier on a table reference against the implicit scope
    /// (spec/design/attached-databases.md §3, Slice 1a). A qualified name reaches a specific database:
    /// `main` (the file / persistent database) or `temp` (the session-local domain) — the two reserved
    /// implicit qualifiers this slice recognizes; a host-attached database arrives in Slice 1b, so any
    /// other qualifier is 42P01 "database … is not attached". Because jed precludes overlaps (a name is
    /// temp XOR persistent within a session, §3), a valid qualifier resolves to the SAME store the bare
    /// name would, so this is a VALIDATION GATE, not a routing change: it asserts the named relation
    /// lives in the claimed database (else 42P01), and the downstream temp-first funnel then resolves it
    /// to the matching scope. A `None` qualifier (a bare, implicit-scope name) always passes. The
    /// qualifier is matched case-insensitively (unquoted identifiers fold to lower case).
    fn check_table_qualifier(&self, qualifier: Option<&str>, name: &str) -> Result<()> {
        let Some(q) = qualifier else {
            return Ok(());
        };
        match q.to_ascii_lowercase().as_str() {
            "temp" => {
                if !self.is_temp_table(name) {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("relation \"temp.{name}\" does not exist"),
                    ));
                }
            }
            "main" => {
                if self.read_snap().table(name).is_none() {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("relation \"main.{name}\" does not exist"),
                    ));
                }
            }
            _ => {
                // A host-attached database (attached-databases.md §5): the qualifier must name an
                // attachment (else 42P01 "database … is not attached") and it must carry the table
                // (else 42P01 "relation … does not exist"). Slice 1a's default case was always 42P01;
                // Slice 1b routes it to the attachment registry.
                let scope = q.to_ascii_lowercase();
                match self.attach_read_snap(&scope) {
                    None => {
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("database \"{q}\" is not attached"),
                        ));
                    }
                    Some(snap) => {
                        if snap.table(name).is_none() {
                            return Err(EngineError::new(
                                SqlState::UndefinedTable,
                                format!("relation \"{q}.{name}\" does not exist"),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Reject a WRITE (DML or DDL) targeting a READ-ONLY host attachment with `25006`
    /// (attached-databases.md §4), before any I/O. A `None` scope, or `main`/`temp` (never read-only via
    /// a qualifier — the read-only *handle* path is separate), or a read-write attachment passes.
    /// Unknown attachments are caught by the qualifier gate, so this only inspects the mode.
    fn check_attachment_writable(&self, scope: Option<&str>) -> Result<()> {
        let Some(q) = scope else { return Ok(()) };
        let Some(core) = &self.core else {
            return Ok(());
        };
        let name = q.to_ascii_lowercase();
        if name == "main" || name == "temp" {
            return Ok(());
        }
        if core.attachment_mode(&name) == Some(crate::shared::AttachMode::ReadOnly) {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                format!("cannot write to read-only database \"{q}\""),
            ));
        }
        Ok(())
    }

    /// Whether this handle's MAIN database is file-backed (durable) rather than in-memory — the input to
    /// the one-durable-writer count (attached-databases.md §5). In the shared-core path the backing path
    /// lives on the core's storage; a standalone engine carries it on `self.path`.
    fn main_is_durable(&self) -> bool {
        match &self.core {
            Some(c) => c.is_file_backed(),
            None => self.path.is_some(),
        }
    }

    /// The page size of a host attachment's OWN page space (attached-databases.md §2) — used to build its
    /// NEW stores (CREATE TABLE / CREATE INDEX) at the size its commit serializes to. A file attachment
    /// carries its own page size, baked into the file, which may differ from main's. The attachment is
    /// known to exist (the qualifier gate passed).
    fn attach_page_size(&self, name: &str) -> u32 {
        self.core
            .as_ref()
            .expect("an attachment write has a shared core")
            .attachment_page_size(name)
    }

    /// The READ snapshot of a host-attached database (attached-databases.md §5) — the transaction's
    /// working clone if this tx wrote it, else the pinned committed root (`attached_committed`). `None`
    /// when no attachment is named `name` (the caller raises 42P01). `name` is expected lowercased.
    fn attach_read_snap(&self, name: &str) -> Option<&Snapshot> {
        if let Some(tx) = &self.session.tx
            && let Some(ws) = tx.attach_working.get(name)
        {
            return Some(ws);
        }
        self.attached_committed.get(name).map(|a| a.as_ref())
    }

    /// The WRITE snapshot of a host-attached database, cloning the pinned committed root into the
    /// transaction's per-attachment working set on first write and marking it dirty (the attachment
    /// analogue of `working_mut`/`temp_working_mut`). A write runs within a transaction, and the
    /// attachment is known to exist (the qualifier gate ran), so this never panics in a correct flow.
    /// `name` is expected lowercased.
    fn attach_write_snap(&mut self, name: &str) -> &mut Snapshot {
        let present = self
            .session
            .tx
            .as_ref()
            .is_some_and(|tx| tx.attach_working.contains_key(name));
        if !present {
            // Clone the committed base BEFORE borrowing `session.tx` mutably (no field-overlap borrow).
            let base = self
                .attached_committed
                .get(name)
                .expect("a write to an attached database resolves its committed root")
                .as_ref()
                .clone();
            let tx = self
                .session
                .tx
                .as_mut()
                .expect("a write statement runs within a transaction");
            tx.attach_working.insert(name.to_string(), base);
        }
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a write statement runs within a transaction");
        tx.attach_dirty.insert(name.to_string());
        tx.attach_working
            .get_mut(name)
            .expect("the working snapshot was just inserted")
    }

    /// The current READ view of every attached database — the transaction's working clone where this tx
    /// wrote it, else the pinned committed root — as one frozen map. Used to freeze a `snapshot_engine`'s
    /// attachment view (whose own tx is `None`, so it reads straight from this map). Returns
    /// `attached_committed` cloned directly when no attachment has been written this tx (the common case).
    fn attach_read_view(&self) -> HashMap<String, std::sync::Arc<Snapshot>> {
        match &self.session.tx {
            Some(tx) if !tx.attach_working.is_empty() => {
                let mut view = self.attached_committed.clone();
                for (k, v) in &tx.attach_working {
                    view.insert(k.clone(), std::sync::Arc::new(v.clone()));
                }
                view
            }
            _ => self.attached_committed.clone(),
        }
    }

    /// The READ snapshot for an explicit database qualifier (attached-databases.md §3): `main` / `temp`
    /// / a host attachment. Used only when a scope is present; a bare (`None`) name keeps the temp-first
    /// funnels. `None` for an unknown attachment (the qualifier gate already raised 42P01).
    ///
    /// This funnel IS where Slice 1c's "temp is an implicit in-memory attachment" reframe is realized
    /// (attached-databases.md §6): `temp`, `main`, and every host attachment resolve through one
    /// scoped-routing path, so a temp table is a citizen of the same mechanism an attachment is. What
    /// stays deliberately distinct is temp's *lifecycle* — it is SESSION-SCOPED (temp_read_snap reads
    /// session-private state; commit lands on the session's temp root with no cross-session roots
    /// publish; its reclamation watermark is the session's open-cursor count, not the Database-wide live
    /// registry). That divergence is correct, not a gap: relocating temp into the Database-scoped
    /// attachment registry would re-share it across sessions (what Slice 0 removed). So temp routes like
    /// an attachment here but keeps its own home.
    fn snap_for_scope(&self, scope: &str) -> Option<&Snapshot> {
        match scope.to_ascii_lowercase().as_str() {
            "temp" => Some(self.temp_read_snap()),
            "main" => Some(self.read_snap()),
            other => self.attach_read_snap(other),
        }
    }

    /// Validate a catalog relation's database qualifier and return the scope string
    /// `snap_for_scope` resolves at exec (introspection.md §5): `None` (unqualified) ⇒ `"main"`
    /// (the implicit scope); `main`/`temp` pass; any other qualifier must name a host attachment
    /// (else `42P01`, the check_table_qualifier wording). Unlike a user table there is no per-table
    /// existence half — the relation exists in EVERY valid scope, so only the scope itself is
    /// validated.
    fn resolve_catalog_scope(&self, qualifier: Option<&str>) -> Result<String> {
        let Some(q) = qualifier else {
            return Ok("main".to_string());
        };
        let lq = q.to_ascii_lowercase();
        if lq == "main" || lq == "temp" {
            return Ok(lq);
        }
        if self.attach_read_snap(&lq).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{q}\" is not attached"),
            ));
        }
        Ok(lq)
    }

    /// Resolve a table's catalog entry honoring an explicit database qualifier (attached-databases.md
    /// §3); a `None` scope keeps the bare temp-first walk.
    fn table_scoped(&self, scope: Option<&str>, name: &str) -> Option<&Table> {
        match scope {
            None => self.table(name),
            Some(q) => self.snap_for_scope(q).and_then(|s| s.table(name)),
        }
    }

    /// A table's READ store honoring an explicit database qualifier; a `None` scope keeps the bare
    /// temp-first funnel. The table is known to exist (resolved upstream).
    fn store_scoped(&self, scope: Option<&str>, name: &str) -> &TableStore {
        match scope {
            None => self.store(name),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_read_snap().store(name),
                "main" => self.read_snap().store(name),
                other => self
                    .attach_read_snap(other)
                    .expect("attachment resolved upstream")
                    .store(name),
            },
        }
    }

    /// A table's WRITE store honoring an explicit database qualifier, marking the right domain dirty
    /// (main / temp / the attachment); a `None` scope keeps the bare temp-first funnel.
    fn store_mut_scoped(&mut self, scope: Option<&str>, name: &str) -> &mut TableStore {
        match scope {
            None => self.store_mut(name),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_working_mut().store_mut(name),
                "main" => self.working_mut().store_mut(name),
                other => {
                    let other = other.to_string();
                    self.attach_write_snap(&other).store_mut(name)
                }
            },
        }
    }

    /// A secondary index's READ store honoring an explicit database qualifier (an index belongs to the
    /// same database as its table); a `None` scope keeps the bare temp-first funnel.
    fn index_store_scoped(&self, scope: Option<&str>, name_key: &str) -> &TableStore {
        match scope {
            None => self.index_store(name_key),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_read_snap().index_store(name_key),
                "main" => self.read_snap().index_store(name_key),
                other => self
                    .attach_read_snap(other)
                    .expect("attachment resolved upstream")
                    .index_store(name_key),
            },
        }
    }

    /// A secondary index's WRITE store honoring an explicit database qualifier; a `None` scope keeps the
    /// bare temp-first funnel.
    fn index_store_mut_scoped(&mut self, scope: Option<&str>, name_key: &str) -> &mut TableStore {
        match scope {
            None => self.index_store_mut(name_key),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_working_mut().index_store_mut(name_key),
                "main" => self.working_mut().index_store_mut(name_key),
                other => {
                    let other = other.to_string();
                    self.attach_write_snap(&other).index_store_mut(name_key)
                }
            },
        }
    }

    /// The `DROP TYPE … RESTRICT` dependency check across every visible scope (spec/design/temp-tables.md
    /// §8): the main image (tables + composite fields), then the visible session-local temp snapshot
    /// (its tables). A composite type is always persistent, but a TEMP table column may reference it, so
    /// dropping the type while such a temp table exists is 2BP01 — matching the persistent case
    /// (PostgreSQL blocks the drop). A session sees only its own session-local temp tables, so the check
    /// is scoped to what is visible (another session's private temp table is invisible by design — and
    /// its resolved [`ColType`] is self-contained, so it keeps working regardless).
    fn composite_dependent_any(&self, name: &str) -> Option<String> {
        self.read_snap()
            .composite_dependent(name)
            .or_else(|| self.temp_read_snap().composite_dependent(name))
    }

    /// Whether `name` is a secondary index on a SESSION-LOCAL temp table (spec/design/temp-tables.md §8)
    /// — the index analogue of [`is_temp_table`](Engine::is_temp_table), used to gate (`allow_temp_ddl`)
    /// and route a `DROP INDEX` of a temp index. Preclude-overlaps keeps an index name in one scope.
    fn is_temp_index(&self, name: &str) -> bool {
        self.temp_read_snap().find_index(name).is_some()
    }

    /// Resolution walk for a sequence by name (spec/design/sequences.md + temp-tables.md §8):
    /// session-local temp → persistent. Preclude-overlaps keeps a name in at most one scope (the shared
    /// relation namespace), so this is just "where the sequence lives". Every sequence READ
    /// (nextval/currval/setval resolution, DROP/ALTER SEQUENCE) goes through here, so a
    /// `serial`/IDENTITY column's OWNED temp sequence resolves exactly like a persistent one.
    fn sequence(&self, name: &str) -> Option<&SequenceDef> {
        if let Some(s) = self.temp_read_snap().sequence(name) {
            return Some(s);
        }
        self.read_snap().sequence(name)
    }

    /// Whether `name` is a sequence in the SESSION-LOCAL temp snapshot (temp-tables.md §8) — the
    /// sequence analogue of [`is_temp_table`](Engine::is_temp_table). A temp sequence only ever
    /// arises from a `serial`/IDENTITY temp column (standalone CREATE SEQUENCE is always persistent),
    /// so it is always owned. Routes a sequence write/gate to the session-local scope.
    fn is_temp_sequence(&self, name: &str) -> bool {
        self.temp_read_snap().sequence(name).is_some()
    }

    /// Stage a sequence def into whichever scope currently owns its name (flagging the matching dirty
    /// bit): session-local temp, else the main working set. A `serial`/IDENTITY temp column's owned
    /// sequence advances (`nextval` flush) into its temp snapshot, so the advance — like the table's
    /// rows — makes zero file writes (temp-tables.md §2). A brand-new persistent sequence is absent from
    /// the temp scope and lands in the main image.
    fn put_sequence_routed(&mut self, def: SequenceDef) {
        if self.is_temp_sequence(&def.name) {
            self.temp_working_mut().put_sequence(def);
        } else {
            self.working_mut().put_sequence(def);
        }
    }

    /// Remove a sequence from whichever scope owns its name (the routed analogue of
    /// [`put_sequence_routed`](Engine::put_sequence_routed)). Used by `DROP SEQUENCE` and
    /// `DROP TABLE`'s owned-sequence auto-drop.
    fn remove_sequence_routed(&mut self, name: &str) {
        let key = name.to_ascii_lowercase();
        if self.is_temp_sequence(name) {
            self.temp_working_mut().remove_sequence(&key);
        } else {
            self.working_mut().remove_sequence(&key);
        }
    }

    /// Rewrite a column's stored DEFAULT expression in whichever scope owns the table — the routed
    /// analogue used by `ALTER SEQUENCE … RENAME` of an owned sequence (temp-tables.md §8), so a
    /// renamed owned TEMP sequence's `nextval` default is rewritten in the temp snapshot.
    fn set_column_default_expr_routed(&mut self, table: &str, col: usize, de: DefaultExpr) {
        if self.is_temp_table(table) {
            self.temp_working_mut()
                .set_column_default_expr(table, col, de);
        } else {
            self.working_mut().set_column_default_expr(table, col, de);
        }
    }

    /// Enforce the per-session temp-table storage budget (`temp_buffers`, spec/design/temp-tables.md
    /// §7) — the §13 gate on RETAINED temp bytes. Checked after each temp-writing statement: if the
    /// session's temp footprint (byte-identical on-disk record bytes, summed over every temp table +
    /// index) **exceeds** the budget, abort `54P03`. The over-budget write is in `temp_working`, so the
    /// abort discards it (autocommit) or fails the block (rolled back at ROLLBACK) — nothing commits.
    /// `temp_buffers = 0` is unlimited; a transaction that did not touch temp cannot have grown it, so
    /// the check self-gates on `temp_dirty` and is a no-op for ordinary (persistent) statements. The
    /// WITHIN-statement bound is `max_cost` (a single huge temp write hits the cost ceiling first).
    /// The `MemoryBlockStore` paging context for the session-local temp domain (temp-tables.md §6),
    /// lazily creating the domain's storage identity ([`Storage::new_temp`] — a private in-RAM store +
    /// pinned pool with within-session compaction on) on first use.
    fn temp_domain_paging(&mut self) -> std::sync::Arc<crate::paging::SharedPaging> {
        if self.temp_storage.is_none() {
            self.temp_storage = Some(crate::shared::Storage::new_temp(self.page_size));
        }
        self.temp_storage.as_ref().unwrap().paging().clone()
    }

    /// Increment [`Engine::open_streams`] and return the RAII guard that decrements it on `Drop`
    /// (bundled into a streaming cursor's pin — shared.rs). While a guard is live a session-local temp
    /// compaction defers (temp-tables.md §6), so a page the cursor may still fault is never reclaimed.
    pub(crate) fn open_stream_guard(&self) -> OpenStreamGuard {
        self.open_streams
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpenStreamGuard {
            count: self.open_streams.clone(),
        }
    }

    fn check_temp_budget(&self) -> Result<()> {
        let limit = self.session.temp_buffers;
        if limit == 0 {
            return Ok(());
        }
        let temp_dirty = self.session.tx.as_ref().is_some_and(|t| t.temp_dirty);
        if !temp_dirty {
            return Ok(());
        }
        // Page-based footprint of the session-local temp domain (temp-tables.md §7, Design decision 3):
        // the committed `MemoryBlockStore` high-water × page size — the honest resident-RAM measure now
        // that temp rides a pager (a record-byte walk would skip demoted `OnDisk` leaves and undercount a
        // multi-leaf temp table, defeating the §13 bound). Deterministic and cross-core-identical:
        // `page_count` is a pure function of operations via the B+tree + within-session compaction. It
        // reflects the state one commit behind (the pending write commits at statement end), so a domain
        // already over budget aborts the NEXT temp write and rolls it back (§7).
        let used = self
            .temp_storage
            .as_ref()
            .map_or(0, |ts| ts.page_count() as u64 * self.page_size as u64);
        if used > limit as u64 {
            return Err(EngineError::new(
                SqlState::TempStorageLimitExceeded,
                format!("temporary table storage exceeded the limit of {limit} bytes"),
            ));
        }
        Ok(())
    }

    /// The committed snapshot, immutable (spec/design/transactions.md §2). Exposed for the host
    /// `Transaction`/read surfaces and for the on-disk serializer.
    pub(crate) fn committed(&self) -> &Snapshot {
        &self.committed
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
    /// row the caller appends. A target table literally named `old`/`new` SHADOWS that
    /// qualifier (the pseudo-relation is suppressed; PostgreSQL's probed rule — its
    /// `WITH (OLD AS o, ...)` aliasing escape stays deferred).
    fn returning(catalog: &'a Engine, table: &'a Table, base_is_old: bool) -> Scope<'a> {
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
        for (pseudo, offset) in [("old", old_offset), ("new", new_offset)] {
            if label != pseudo {
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

#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
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
    /// boolean item). Also the `@@` operator.
    Match,
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
    /// from another datetime family — the runtime `Value` carries the source family. Casts crossing
    /// the `timestamptz` boundary consult the session zone (charging `timezone`); `±infinity` and
    /// NULL pass through. `to` is one of `Timestamp` / `Timestamptz` / `Date`.
    DateConvert {
        inner: Box<RExpr>,
        to: ScalarType,
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
fn count_returning_refs(returning: &Option<SelectItems>, name: &str) -> usize {
    match returning {
        Some(SelectItems::Items(items)) => items
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
            ],
        ),
        // SrfKind::JedConstraints
        _ => table(
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
    /// INNER/CROSS join (cost.md §3 "JOIN"): the nested loop drives the outer in PK order, so its
    /// output is already in order — the sort is elided and a `LIMIT` short-circuits the loop. Set only
    /// for exactly two non-lateral base relations, a `LIMIT`, and a forward outer-PK `ORDER BY`.
    join_pk_ordered: bool,
    /// Scan-bound pushdown, **one entry per relation** in `rels`: the WHERE conjuncts that
    /// bound that relation's scan — a primary-key range, or (when no PK bound applies) a
    /// secondary-index equality (cost.md §3 "bounded scan" / "index-bounded scan"). `None` ⇒
    /// a full scan of that relation. In a JOIN each base table is bounded independently by
    /// the WHERE predicates against a CONSTANT (literal/param/outer) — a cross-relation
    /// `b.pk = a.x` is the index-nested-loop case (still a follow-on; `const_source` rejects
    /// a sibling column). The residual filter stays the WHOLE `filter`, re-applied after the
    /// join — the bound only narrows which rows are scanned.
    rel_bounds: Vec<Option<ScanBound>>,
    /// **Index-nested-loop** scan bounds, one per relation (cost.md §3 "JOIN"). `Some` for a join
    /// inner relation whose primary key / indexed column is compared to a **sibling** column of an
    /// earlier relation (`a JOIN b ON b.pk = a.x`) — a `BoundSrc::Sibling` bound resolved per outer
    /// row from the combined left-hand row. When set, that relation is NOT materialized once up
    /// front; the join loop re-materializes it per left row (like a correlated `LATERAL`), seeking
    /// instead of full-scanning — O(N·M) → O(N·log M). `None` ⇒ the ordinary once-materialized
    /// `rel_bounds` path. A set entry takes precedence over `rel_bounds` for that relation.
    rel_inl_bounds: Vec<Option<ScanBound>>,
    /// The **touched set** per relation (cost.md §3 "The touched set"; large-values.md §14): which
    /// of its columns this query statically references. Drives the chain-`page_read` /
    /// `value_decompress` portion of the scan's up-front cost block — an untouched spilled or
    /// compressed column charges nothing, however many records the bound admits.
    rel_masks: Vec<Vec<bool>>,
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

/// The plan-time result of PK analysis: the PK's storage type + the bound terms. The concrete key
/// range is built per execution by `build_key_bound`.
pub(crate) struct PkBound {
    pk_type: ScalarType,
    terms: Vec<BoundTerm>,
    /// The key column's resolved collation when it is collated AND `Full` (loaded version matches
    /// the file's pin) — the probe encodes via this collation's UCA sort key (encoding.md §2.12), so
    /// it seeks the same key FORM the B-tree stores (spec/design/collation.md §8). `None` for a `C`
    /// (raw-byte) key. A `Skewed` collated key never produces a `PkBound` at all (`key_collation_ctx`
    /// refuses the bound — collation.md §12), so this is `Some` only for a safe-to-seek collated key.
    coll: Option<std::sync::Arc<Collation>>,
}

/// The plan-time result of an OR / IN-list disjunction of primary-key equalities
/// (`pk = a OR pk = b OR …`, or the equivalent `pk IN (a, b, …)` which desugars to that OR-chain
/// — cost.md §3 "OR / IN-list"). `srcs` is the equality const-sources, one per disjunct, in source
/// order (a bind param, an outer/correlated column, or a literal — a const-source of the PK type).
/// At exec time each src encodes into the PK key space; the resulting keys are de-duplicated and
/// sorted, and each becomes a point probe `[k, k]`. The whole WHERE stays the residual filter (the
/// union is a superset), so the result is unchanged. `coll` is the PK's key collation (`None` for a
/// `C` key), as in [`PkBound`].
pub(crate) struct PkKeySet {
    pk_type: ScalarType,
    coll: Option<std::sync::Arc<Collation>>,
    srcs: Vec<BoundSrc>,
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
    srcs: Vec<BoundSrc>,
}

/// A per-relation scan bound (cost.md §3): a primary-key range, a secondary-index
/// equality (spec/design/indexes.md §5), a GIN-bounded scan over an array column
/// (spec/design/gin.md §6), a GiST-bounded scan, or a MERGED point-set (an OR / IN-list of key
/// equalities lowered to a union of point probes — cost.md §3 "OR / IN-list"). The PK bound wins
/// when several apply — it is the row's own key (no second tree, range-capable, strictly cheaper);
/// the ordered-index equality bound wins over GIN (the deterministic precedence, gin.md §6). The
/// point-set bounds (`PkSet`/`IndexSet`) are a LAST-RESORT access path, chosen only when no
/// contiguous PK/index/GIN/GiST bound applies, so they never displace an existing plan.
pub(crate) enum ScanBound {
    Pk(PkBound),
    Index(IndexBound),
    Gin(GinBound),
    Gist(GistBound),
    PkSet(PkKeySet),
    IndexSet(IndexKeySet),
}

impl ScanBound {
    /// Whether this bound needs the general eager materialize path (`materialize_rel` / the DML
    /// scan) rather than a single-contiguous-range fast path (streaming scan, columnar project,
    /// vectorized aggregate, streaming sort, join top-N). True for a second-tree gather
    /// (index / GIN / GiST) and for a merged OR/IN point-set (`PkSet` / `IndexSet` — a union of
    /// probes, cost.md §3 "OR / IN-list"); false for a plain PK contiguous bound (which every fast
    /// path handles via a single `build_key_bound`). Every single-table fast-path gate consults
    /// this so the point-set bounds are interpreted in exactly ONE place (`materialize_rel`), never
    /// silently dropped to a full scan by a fast path that only understands `Pk`.
    fn needs_eager_scan(&self) -> bool {
        matches!(
            self,
            ScanBound::Index(_)
                | ScanBound::Gin(_)
                | ScanBound::Gist(_)
                | ScanBound::PkSet(_)
                | ScanBound::IndexSet(_)
        )
    }
}

/// The plan-time result of GiST analysis (spec/design/gist.md §5): the chosen GiST index (lowest
/// lowercased name whose range column has a `col && const` / `col @> const` conjunct), the operator
/// strategy, and the column's global scope index. Like [`GinBound`], the constant query operand is
/// NOT stored (re-found in `plan.filter` at exec time by `gist_match`). No element type is carried:
/// the gather descends the resident R-tree (gist.md §4.1), whose bounds are already decoded.
pub(crate) struct GistBound {
    /// The index store's key — the lowercased index name (its resident R-tree lives under this key).
    name_key: String,
    strategy: crate::gist::GistStrategy,
    /// The GiST-indexed column's global scope index (`rel.offset + ci`).
    col_global: usize,
    /// `Some(scalar)` for the scalar `=` opclass (GX2): the column's scalar type, so `gist_bound_rows`
    /// can encode the equality constant to its order-preserving key bytes. `None` for `range_ops`,
    /// whose `&&`/`@>` query is a range constant the resident R-tree compares directly.
    scalar_type: Option<ScalarType>,
}

/// Which array operator a GIN bound accelerates (spec/design/gin.md §6): `@>` (contains, mode
/// ALL → posting-list intersection), `&&` (overlaps, mode ANY → posting-list union), or
/// `= ANY` (membership — `c = ANY(col)`, the single-term `@>` reduction: one scalar term, mode
/// ALL → its lone posting list).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GinStrategy {
    Contains,
    Overlaps,
    /// `c = ANY(col)` — `c` is a constant SCALAR (not an array); its single term is gathered like
    /// a one-element `@>`. The query operand recovered by `gin_match` is the scalar `c`.
    Member,
    /// `col = Q` — exact array equality. The query operand is the constant array `Q`; its distinct
    /// non-NULL elements gather the SAME candidate superset as `@> Q` (equal arrays have identical
    /// element multisets, so `col = Q` ⟹ `col @> Q`), and the residual `=` filter makes it exact.
    /// Unlike `Contains`, a NULL ELEMENT of `Q` does not empty the bound; and a `Q` with no non-NULL
    /// element (`'{}'`/all-NULL) falls back to the full scan, not a provably-empty bound (gin.md §6).
    Equal,
}

/// The plan-time result of GIN analysis (spec/design/gin.md §6): the chosen GIN index (lowest
/// lowercased name whose array column has a `col @> const` / `col && const` conjunct), the array
/// **element** type (for `encode_element` — the term bytes), the operator strategy, and the
/// column's global scope index. The constant query `Q` is NOT stored (`RExpr` is not `Clone`); it
/// is re-found in `plan.filter` at exec time by `gin_match` and evaluated there.
pub(crate) struct GinBound {
    /// The index store's key — the lowercased index name.
    name_key: String,
    /// The array element type, whose key encoding produces each term's bytes.
    elem_type: ScalarType,
    strategy: GinStrategy,
    /// The GIN-indexed column's global scope index (`rel.offset + ci`).
    col_global: usize,
}

/// One column of an index access predicate's equality prefix (indexes.md §5.1): the column's
/// storage type, its key collation (`Some(coll)` only for a `Full`-collated text column), and every
/// equality const-source bound to it. At exec time the sources must agree on one value (else the
/// bound is provably empty). A collated column encodes its probe via the UCA sort key
/// (encoding.md §2.12) to match the index's stored key form (collation.md §8).
pub(crate) struct IndexEqCol {
    col_type: ScalarType,
    coll: Option<std::sync::Arc<Collation>>,
    srcs: Vec<BoundSrc>,
}

/// The optional trailing range of an index access predicate (indexes.md §5.1): a range on the key
/// column immediately after the equality prefix. Its column is fixed-width (never collated).
pub(crate) struct IndexRange {
    col_type: ScalarType,
    terms: Vec<BoundTerm>,
}

/// The plan-time result of index analysis (indexes.md §5.1): the chosen index (lowest lowercased
/// name yielding a non-empty access predicate) and the predicate — a maximal EQUALITY PREFIX on the
/// leading key columns (`eq_cols`) plus an OPTIONAL RANGE on the next column (`range`). At exec time
/// `build_index_bound` turns these into a concrete index-key range: the equality prefix bytes
/// P = concatenated present slots, then the range (if any) intersected relative to P.
/// `suffix_types` are the types of the index columns AFTER the equality prefix (`columns[eq..]`) —
/// the range column (if any) plus every trailing column — each FIXED-WIDTH so an admitted entry's
/// row-key suffix is recovered by width-skipping them past P.
pub(crate) struct IndexBound {
    /// The index store's key — the lowercased index name.
    name_key: String,
    eq_cols: Vec<IndexEqCol>,
    range: Option<IndexRange>,
    suffix_types: Vec<ScalarType>,
}

/// The outcome of encoding a const-source into the PK key space.
pub(crate) enum BoundKey {
    /// A NULL const — the comparison is 3VL-unknown, so the range is provably empty.
    Null,
    /// An integer value outside the PK type's range — no key can equal it, so drop this half-bound.
    OutOfRange,
    Key(Vec<u8>),
}

/// Construct an index access predicate for `idx` over `rel` (indexes.md §5.1): a maximal EQUALITY
/// PREFIX on the leading key columns plus an OPTIONAL RANGE on the next column. It walks the index's
/// key columns in key order against the WHERE AND-chain, consuming a column with an agreed equality
/// conjunct into the prefix and stopping at the first column that has no equality (taking its range
/// conjuncts, if any, as the trailing range). Returns `None` for a non-B-tree index, a `Skewed`
/// collated bound column (whose stored keys are at the file's pinned version — collation.md §12), no
/// bound at all, or an ineligible suffix (a column after the equality prefix that is not a
/// fixed-width scalar — the width-based key-suffix skip needs it). `sibling_cutoff` opens the
/// index-nested-loop door (`Some(cut)` admits a bare sibling `Column(g)` with `g < cut` as a bound
/// source, resolved per outer row); `None` is the ordinary once-materialized bound.
fn build_index_access_predicate(
    filter: &RExpr,
    rel: &ScopeRel,
    idx: &IndexDef,
    sibling_cutoff: Option<usize>,
    catalog: &Engine,
) -> Option<IndexBound> {
    if idx.kind != IndexKind::Btree {
        return None;
    }
    let mut eq_cols: Vec<IndexEqCol> = Vec::new();
    let mut range: Option<IndexRange> = None;
    for &ci in &idx.columns {
        // A non-scalar (range/array/composite) column cannot be seeked here — stop the prefix.
        let Some(ty) = rel.table.columns[ci].ty.as_scalar() else {
            break;
        };
        // The column's key collation form (collation.md §8/§12): a `Skewed` collated column refuses
        // the bound (its stored keys are wrong for the loaded bundle) — stop the prefix here (the
        // column then falls into the fixed-width suffix check below and rejects the whole index if it
        // is text). A `C` or `Full`-collated column is admissible.
        let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[ci]) else {
            break;
        };
        let mut terms = Vec::new();
        collect_bound_terms(
            filter,
            rel.offset + ci,
            ty,
            coll.as_ref().map(|c| c.name.as_str()),
            sibling_cutoff,
            &mut terms,
        );
        let (eqs, ranges): (Vec<BoundTerm>, Vec<BoundTerm>) =
            terms.into_iter().partition(|t| matches!(t.op, CmpOp::Eq));
        if !eqs.is_empty() {
            eq_cols.push(IndexEqCol {
                col_type: ty,
                coll,
                srcs: eqs.into_iter().map(|t| t.src).collect(),
            });
            continue; // extend the equality prefix
        }
        if !ranges.is_empty() {
            range = Some(IndexRange {
                col_type: ty,
                terms: ranges,
            });
        }
        break; // first non-equality column ends the prefix (with or without a trailing range)
    }
    if eq_cols.is_empty() && range.is_none() {
        return None; // nothing bound
    }
    // Eligibility: every index column from the range column onward (`columns[eq_cols.len()..]`) is
    // width-skipped past the known equality prefix, so each must be a fixed-width scalar. The
    // equality-prefix columns may be any width — their slots are matched as the known prefix bytes.
    let mut suffix_types = Vec::with_capacity(idx.columns.len() - eq_cols.len());
    for &c in &idx.columns[eq_cols.len()..] {
        let s = rel.table.columns[c].ty.as_scalar()?;
        if !s.is_fixed_width() {
            return None;
        }
        suffix_types.push(s);
    }
    Some(IndexBound {
        name_key: idx.name.to_ascii_lowercase(),
        eq_cols,
        range,
        suffix_types,
    })
}

/// Pick one relation's scan bound (cost.md §3; indexes.md §5): the single-column PK bound
/// first (the row's own key — range-capable and strictly cheaper); else, among the
/// relation's indexes (held in ascending lowercased-name order — the deterministic
/// tie-break), the first that yields a non-empty access predicate
/// ([`build_index_access_predicate`]); else `None` (full scan).
fn detect_scan_bound(filter: &RExpr, rel: &ScopeRel, catalog: &Engine) -> Option<ScanBound> {
    // A host-attached relation full-scans this slice (attached-databases.md §8): the bounded-scan exec
    // path resolves index stores UNSCOPED, so no PK/index/GiST/GIN bound may apply to an attachment.
    if rel.is_attachment() {
        return None;
    }
    if let Some(b) = rel.table.primary_key_index().and_then(|pk_local| {
        // Ordered-equality pushdown is scalar-only; a non-scalar (range) PK skips it (point-lookup
        // deferred for containers — ranges.md §10), falling through to a full scan + residual filter.
        let sty = rel.table.columns[pk_local].ty.as_scalar()?;
        // The PK column's key collation form (collation.md §8/§12): `Some(None)` = `C` (raw-byte
        // key); `Some(Some(coll))` = collated AND `Full` (push via the sort key); `None` = collated
        // but `Skewed` ⇒ refuse pushdown (full heap-scan recompute — the read-safety rule §12).
        let coll = key_collation_ctx(catalog, &rel.table.columns[pk_local])?;
        detect_pk_bound(filter, rel.offset + pk_local, sty, coll)
    }) {
        return Some(ScanBound::Pk(b));
    }
    for idx in &rel.table.indexes {
        // An index access predicate (indexes.md §5.1): a maximal equality prefix + optional trailing
        // range over a B-tree index's leading key columns. Returns `None` for a GIN/GiST index
        // (handled by the passes below), an ineligible suffix, or no bound. Indexes are held in
        // ascending lowercased-name order, so the first `Some` wins — the deterministic tie-break.
        if let Some(ib) = build_index_access_predicate(filter, rel, idx, None, catalog) {
            return Some(ScanBound::Index(ib));
        }
    }
    // GiST bound (gist.md §5) — a `col && const` / `col @> const` over a range column; the ordered
    // loop above already skipped the GiST index (its leading column is a non-scalar range).
    if let Some(gb) = detect_gist_bound(filter, &rel.table.indexes, &rel.table.columns, rel.offset)
    {
        return Some(ScanBound::Gist(gb));
    }
    // GIN bound (gin.md §6) — after the PK and ordered-index equality bounds.
    if let Some(gb) = detect_gin_bound(filter, &rel.table.indexes, &rel.table.columns, rel.offset) {
        return Some(ScanBound::Gin(gb));
    }
    // LAST RESORT — an OR / IN-list of key equalities lowered to merged point probes (cost.md §3
    // "OR / IN-list"). Reached only when no contiguous PK/index/GIN/GiST bound applied above, so
    // this never displaces an existing plan. The primary key wins over a secondary index (its own
    // key — no second tree), matching `detect_scan_bound`'s PK-then-index ordering.
    if let Some(b) = rel.table.primary_key_index().and_then(|pk_local| {
        let sty = rel.table.columns[pk_local].ty.as_scalar()?;
        let coll = key_collation_ctx(catalog, &rel.table.columns[pk_local])?;
        let srcs = detect_key_set(filter, rel.offset + pk_local, sty, coll.as_deref())?;
        Some(ScanBound::PkSet(PkKeySet {
            pk_type: sty,
            coll,
            srcs,
        }))
    }) {
        return Some(b);
    }
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Btree {
            continue;
        }
        let ci = idx.columns[0];
        let Some(ty) = rel.table.columns[ci].ty.as_scalar() else {
            continue;
        };
        if idx.columns[1..].iter().any(|&c| {
            rel.table.columns[c]
                .ty
                .as_scalar()
                .is_none_or(|s| !s.is_fixed_width())
        }) {
            continue;
        }
        let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[ci]) else {
            continue;
        };
        if let Some(srcs) = detect_key_set(filter, rel.offset + ci, ty, coll.as_deref()) {
            return Some(ScanBound::IndexSet(IndexKeySet {
                name_key: idx.name.to_ascii_lowercase(),
                col_type: ty,
                coll,
                tail_types: idx.columns[1..]
                    .iter()
                    .map(|&c| rel.table.columns[c].ty.scalar())
                    .collect(),
                srcs,
            }));
        }
    }
    None
}

/// Find an OR / IN-list disjunction of equalities on ONE key column (at global index `key_idx`) and
/// return its equality const-sources (one per disjunct), or `None` if the filter has no such shape
/// (cost.md §3 "OR / IN-list"). `x IN (a, b, c)` desugars to `x = a OR x = b OR x = c` at resolve
/// time (grammar.md §20), so an IN-list and a hand-written OR-of-equalities present the identical
/// tree — this handles both. The filter's top-level AND-chain is flattened; the FIRST conjunct that
/// reduces to a pure disjunction of `keycol = const` equalities is used (the rest of the WHERE stays
/// the residual filter). A conjunct reduces iff it is a `keycol = const`, or an OR of two reducing
/// operands — an AND, a NOT, a range comparison, or an equality on a different column makes it
/// non-reducing, so a mixed disjunction (`pk = 1 OR x = 2`) or a NOT IN (`NOT (pk = 1 OR …)`)
/// correctly yields no bound. Conservative + sound: an unrecognized filter contributes no bound.
fn detect_key_set(
    filter: &RExpr,
    key_idx: usize,
    key_type: ScalarType,
    coll: Option<&Collation>,
) -> Option<Vec<BoundSrc>> {
    let col_coll = coll.map(|c| c.name.as_str());
    // Walk the top-level AND chain; the FIRST conjunct that reduces to a pure disjunction of
    // `keycol = const` equalities is used (the rest of the WHERE stays the residual filter).
    fn walk(
        e: &RExpr,
        key_idx: usize,
        key_type: ScalarType,
        col_coll: Option<&str>,
    ) -> Option<Vec<BoundSrc>> {
        if let RExpr::And(l, r) = e {
            return walk(l, key_idx, key_type, col_coll)
                .or_else(|| walk(r, key_idx, key_type, col_coll));
        }
        reduce_key_set(e, key_idx, key_type, col_coll)
    }
    walk(filter, key_idx, key_type, col_coll)
}

/// Reduce one predicate to the set of equality const-sources it bounds `key_idx` with, or `None` if
/// it is not a pure disjunction of `keycol = const` ([`detect_key_set`]). Descends OR nodes only; a
/// single `keycol = const` leaf is the base case (the same term extraction as
/// [`collect_bound_terms`], with no sibling references — a once-materialized bound). A comparison
/// bounds the key only when ITS resolved collation matches the key column's frozen collation.
fn reduce_key_set(
    e: &RExpr,
    key_idx: usize,
    key_type: ScalarType,
    col_coll: Option<&str>,
) -> Option<Vec<BoundSrc>> {
    if let RExpr::Or(l, r) = e {
        let mut left = reduce_key_set(l, key_idx, key_type, col_coll)?;
        let right = reduce_key_set(r, key_idx, key_type, col_coll)?;
        left.extend(right);
        return Some(left);
    }
    if let RExpr::Compare {
        op: CmpOp::Eq,
        lhs,
        rhs,
        collation,
    } = e
    {
        if collation.as_ref().map(|c| c.name.as_str()) != col_coll {
            return None;
        }
        let is_key = |x: &RExpr| matches!(x, RExpr::Column(i) if *i == key_idx);
        let src = if is_key(lhs) {
            const_source(rhs, key_type, None)
        } else if is_key(rhs) {
            const_source(lhs, key_type, None)
        } else {
            None
        };
        return src.map(|s| vec![s]);
    }
    None
}

/// Detect an **index-nested-loop** scan bound for a join inner relation `rel` (spec/design/cost.md
/// §3 "JOIN"): a primary-key (or leading secondary-index column) comparison to a **sibling** column
/// of an EARLIER join relation, taken from the join's `on` predicate OR the `where` filter. Unlike
/// [`detect_scan_bound`] (constants only), this admits a bare sibling column (`BoundSrc::Sibling`,
/// enabled by `sibling_cutoff = rel.offset`), resolved per outer row from the current combined
/// left-hand row — the join analog of a correlated subquery's outer reference
/// (`query.correlated_pushdown`). So the inner relation seeks per outer row instead of full-scanning
/// for every outer row: O(N·M) → O(N·log M).
///
/// Returns `Some` only when the resulting bound has **≥ 1 sibling term** — a constant-only bound is
/// the ordinary once-materialized `rel_bounds` path, not index-nested-loop. Constant terms on the
/// same key that co-occur (`b.pk = a.x AND b.pk = 5`) ride along and tighten the per-outer-row seek.
/// The whole `on`/`where` stays the residual filter (the bound is a superset of the matching rows),
/// so the **rows are unchanged**; only the inner re-scan cost drops. Caller restricts this to a base
/// table that is the right/nullable side of an INNER/CROSS/LEFT join (a RIGHT/FULL preserved side
/// cannot be bounded per outer row — it would drop rows matching no outer row).
fn detect_inl_bound(
    on: Option<&RExpr>,
    where_filter: Option<&RExpr>,
    rel: &ScopeRel,
    catalog: &Engine,
) -> Option<ScanBound> {
    // A host-attached inner relation full-scans per outer row this slice (attached-databases.md §8):
    // the seek would resolve its index store unscoped. Index-nested-loop over an attachment is a
    // perf follow-on.
    if rel.is_attachment() {
        return None;
    }
    let cutoff = Some(rel.offset);
    // Collect the key's bound terms from BOTH the ON and the WHERE (a NULL predicate contributes
    // none), with sibling columns admitted.
    let collect = |key_idx: usize, ty: ScalarType, ccoll: Option<&str>| -> Vec<BoundTerm> {
        let mut terms = Vec::new();
        if let Some(f) = on {
            collect_bound_terms(f, key_idx, ty, ccoll, cutoff, &mut terms);
        }
        if let Some(f) = where_filter {
            collect_bound_terms(f, key_idx, ty, ccoll, cutoff, &mut terms);
        }
        terms
    };
    // Primary-key bound first (the row's own key — range-capable, strictly cheaper).
    if let Some(b) = rel.table.primary_key_index().and_then(|pk_local| {
        let sty = rel.table.columns[pk_local].ty.as_scalar()?;
        let coll = key_collation_ctx(catalog, &rel.table.columns[pk_local])?;
        let terms = collect(
            rel.offset + pk_local,
            sty,
            coll.as_ref().map(|c| c.name.as_str()),
        );
        terms
            .iter()
            .any(|t| matches!(t.src, BoundSrc::Sibling(_)))
            .then(|| {
                ScanBound::Pk(PkBound {
                    pk_type: sty,
                    terms,
                    coll,
                })
            })
    }) {
        return Some(b);
    }
    // Else a leading secondary-index equality bound to a sibling (indexes held in ascending
    // lowercased-name order — the deterministic tie-break, matching detect_scan_bound).
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Btree {
            continue;
        }
        let ci = idx.columns[0];
        let Some(ty) = rel.table.columns[ci].ty.as_scalar() else {
            continue;
        };
        if idx.columns[1..].iter().any(|&c| {
            rel.table.columns[c]
                .ty
                .as_scalar()
                .is_none_or(|s| !s.is_fixed_width())
        }) {
            continue;
        }
        let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[ci]) else {
            continue;
        };
        let terms = collect(rel.offset + ci, ty, coll.as_ref().map(|c| c.name.as_str()));
        let eqs: Vec<BoundSrc> = terms
            .into_iter()
            .filter(|t| matches!(t.op, CmpOp::Eq))
            .map(|t| t.src)
            .collect();
        if eqs.iter().any(|s| matches!(s, BoundSrc::Sibling(_))) {
            // This slice keeps the index-nested-loop bound single-column-equality (a leading key
            // column bound to a sibling); a multi-column / range INL bound is a follow-on (cost.md
            // §3 "index-nested-loop"). `suffix_types` are the trailing columns (columns[1..],
            // fixed-width by the check above), width-skipped past the single equality slot.
            return Some(ScanBound::Index(IndexBound {
                name_key: idx.name.to_ascii_lowercase(),
                eq_cols: vec![IndexEqCol {
                    col_type: ty,
                    coll,
                    srcs: eqs,
                }],
                range: None,
                suffix_types: idx.columns[1..]
                    .iter()
                    .map(|&c| rel.table.columns[c].ty.scalar())
                    .collect(),
            }));
        }
    }
    None
}

/// The collation a key over `col` is STORED under, deciding whether — and how — a comparison bound
/// may push down to that key (spec/design/collation.md §8/§12). Three outcomes:
///   - `Some(None)`       — `col` is `C` (or non-text): the key is raw bytes (encoding.md §2.4),
///                          always pushable, the unchanged fast path.
///   - `Some(Some(coll))` — `col` is collated and the collation is `Full` (its file pin matches the
///                          loaded bundle): the key is the UCA sort key (encoding.md §2.12), pushable
///                          using `coll` to encode the probe in the same form.
///   - `None`             — `col` is collated but `Skewed` (the file's keys are at a DIFFERENT
///                          `(unicode, cldr)` than the loaded bundle provides): pushdown is REFUSED.
///                          The scan stays a full heap-scan that recomputes against the LOADED table
///                          (the read-safety rule §12; seeking a loaded-version probe in a
///                          file-version B-tree would mis-match — the regression tripwire
///                          suites/collation/skew.test stays green only because this refuses). An
///                          unresolvable collation likewise refuses rather than mis-encoding.
fn key_collation_ctx(catalog: &Engine, col: &Column) -> Option<Option<std::sync::Arc<Collation>>> {
    match &col.collation {
        None => Some(None),
        Some(name) => {
            let snap = catalog.read_snap();
            if snap.collation_skew(name).is_some() {
                None
            } else {
                snap.resolve_collation(name).map(Some)
            }
        }
    }
}

/// Whether a single base relation's `ORDER BY` is satisfied **by its primary-key scan order**
/// (spec/design/cost.md §3 "ORDER BY satisfied by primary-key order") — i.e. the table tree, walked
/// forward in storage-key order, already delivers rows in the requested order, so the sort is a
/// no-op. True iff the `ORDER BY` keys are a **prefix of the PK columns** (in key order), each
/// `ASC` (a `DESC` reverse scan is a follow-on) and sorting by the **same order the stored PK key
/// realizes** (collation.md §8/§12). The PK columns are NOT NULL, so a key's `NULLS FIRST|LAST` is
/// a no-op (no NULLs to place) and is ignored. Two coverage shapes both qualify: an `ORDER BY`
/// shorter than the PK is a prefix (ties are broken by the remaining PK columns — the canonical PK
/// tie-break, matching the eager stable sort); an `ORDER BY` longer than the PK matches the whole
/// PK and its extra keys are redundant (the PK is unique, so there are no ties left to break).
/// Reports whether a single base relation's `ORDER BY` is satisfied by its PRIMARY-KEY scan order
/// (spec/design/cost.md §3), and in which **direction** — `Some(false)` for a forward (`ASC`) scan,
/// `Some(true)` for a reverse (`DESC`) scan, `None` when the sort cannot be elided.
///
/// The direction is taken from the first `ORDER BY` key; every PK-prefix key must share it (a mixed
/// `ASC`/`DESC` order is no pure scan direction). Two asymmetric coverage rules, both grounded in the
/// eager sort being a **stable sort that breaks ties in input = PK-ascending order**:
/// - **Forward (`ASC`)** allows a strict **prefix** of the PK — the remaining PK columns tie-break
///   ascending, exactly the input order the stable sort preserves (so the forward scan's
///   continuation matches).
/// - **Reverse (`DESC`)** requires the **full PK** (`order.len() >= pk.len()`): a strict DESC prefix
///   of a composite PK would have the eager sort break ties in PK-**ascending** input order, which a
///   reverse scan inverts — so reverse is restricted to the unique full key, where no ties remain.
fn order_satisfied_by_pk(
    table: &Table,
    offset: usize,
    order: &[crate::spill::SortKey],
    catalog: &Engine,
) -> Option<bool> {
    let pk = table.pk_indices();
    if pk.is_empty() {
        return None; // no PK (synthetic rowid order is not a user-visible column)
    }
    let reverse = order[0].1; // direction comes from the first ORDER BY key's `descending` flag
    if reverse && order.len() < pk.len() {
        return None; // a reverse scan needs the full (unique) PK so no ties remain (see above)
    }
    let m = order.len().min(pk.len());
    for (i, (slot, descending, _nulls_first, coll)) in order.iter().take(m).enumerate() {
        if *descending != reverse {
            return None; // every PK-prefix key must share the scan direction (no mixed ASC/DESC)
        }
        if *slot != offset + pk[i] {
            return None; // must be the i-th PK column, in key order
        }
        // The ORDER BY key must sort by the SAME order the stored PK key realizes. A raw-byte
        // (`C`/non-text) key matches a key with no collation; a `Full`-collated key matches the
        // SAME collation; a `Skewed`/unresolvable collation never matches (the stored keys are at
        // the file's pinned version, so the scan order would be wrong for the loaded one — the
        // read-safety rule §12; recompute via the eager/streaming sort instead).
        match key_collation_ctx(catalog, &table.columns[pk[i]]) {
            None => return None,
            Some(None) => {
                if coll.is_some() {
                    return None;
                }
            }
            Some(Some(c)) => match coll {
                Some(c2) if c2.name == c.name => {}
                _ => return None,
            },
        }
    }
    Some(reverse)
}

/// Whether a frame folds only rows at or before the current row in the scan order (spec/design/
/// window.md §5.2/§6). The frame END must not look forward; a RANGE/GROUPS CURRENT-ROW end spans the
/// current peer group, which pulls in later rows unless the ordering key is unique. A ROWS frame uses
/// physical position, so it never expands to peers. The default frame (`None`, with a window ORDER BY)
/// is RANGE UNBOUNDED PRECEDING TO CURRENT ROW — safe only when the key is unique.
fn frame_backward_safe(frame: &Option<ResolvedFrame>, unique: bool) -> bool {
    let Some(frame) = frame else {
        return unique;
    };
    match &frame.end {
        // Strictly before the current peer group.
        ResolvedBound::UnboundedPreceding | ResolvedBound::Preceding(_) => true,
        // ROWS = the physical current row; RANGE/GROUPS = the current peer group (forward peers unless
        // the key is unique).
        ResolvedBound::CurrentRow => matches!(frame.mode, crate::ast::FrameMode::Rows) || unique,
        // Look forward.
        ResolvedBound::Following(_) | ResolvedBound::UnboundedFollowing => false,
    }
}

/// The fixed byte width of a table's stored primary key (`encode_pk_key` = the bare per-column
/// order-preserving keys concatenated, no NULL tags — a PK is `NOT NULL`), or `None` when ANY PK
/// column is variable-width (`text`/`decimal`/`bytea`/`interval`) or non-scalar (range/composite),
/// or the table has no PK. Used by the secondary-index-order scan to **peel the PK suffix off the
/// END of each index entry key** (the "key-suffix skip", cost.md §3) — sound only when that suffix
/// is a known fixed length, which is exactly when this returns `Some`.
fn pk_storage_width(table: &Table) -> Option<usize> {
    let pk = table.pk_indices();
    if pk.is_empty() {
        return None; // a no-PK table keys on a synthetic rowid — not handled this slice
    }
    let mut w = 0usize;
    for &ci in &pk {
        let s = table.columns[ci].ty.as_scalar()?; // a non-scalar (range/composite) PK has no fixed width
        if !s.is_fixed_width() {
            return None; // a variable-width (text/decimal/…) PK suffix is not a fixed peel
        }
        w += s.width_bytes();
    }
    Some(w)
}

/// The secondary-index-order plan: walk a B-tree index in key order to satisfy an `ORDER BY` without
/// a sort, point-looking-up each row by its primary key (cost.md §3 "secondary-index order").
pub(crate) struct IndexOrder {
    /// The index store's key — the lowercased index name.
    name_key: String,
    /// The fixed byte width of the PK suffix to peel off the END of each index entry key
    /// ([`pk_storage_width`]) — the row's storage key, fed to the table point lookup.
    pk_width: usize,
}

/// Reports whether a single base relation's `ORDER BY` is satisfied by walking one of its **B-tree
/// secondary indexes** in key order (cost.md §3 "secondary-index order"), and which index. The index
/// store holds its entries in `(indexed columns, storage key)` order, so a forward walk delivers rows
/// in `ORDER BY <indexed columns> ASC NULLS LAST` order, ties broken by the PK — exactly the eager
/// stable sort's tie-break.
///
/// Returns `Some` iff the `ORDER BY` keys are **exactly** a B-tree index's columns (same count, same
/// columns in key order), each `ASC` with **default `NULLS LAST`** (the index stores `NULL` as `0x01`
/// after a present `0x00`, so it realizes NULLS-LAST; an explicit `NULLS FIRST` does not match) and
/// sorting by the column's stored key collation (`Skewed`/unresolvable → refuse, the §12 read-safety
/// rule), **and** the table's PK is fixed-width ([`pk_storage_width`]). The exact-match requirement is
/// load-bearing: a strict prefix of a *multi*-column index would tie-break by the remaining index
/// columns rather than the PK, diverging from the eager sort (the same tie-break trap the
/// composite-PK reverse case carries). `DESC` (a reverse index walk) is a follow-on.
fn order_satisfied_by_index(
    table: &Table,
    offset: usize,
    order: &[crate::spill::SortKey],
    catalog: &Engine,
) -> Option<IndexOrder> {
    let pk_width = pk_storage_width(table)?;
    for idx in &table.indexes {
        if idx.kind != IndexKind::Btree {
            continue; // only an ordered B-tree realizes the column order (GIN/GiST do not)
        }
        if order.len() != idx.columns.len() {
            continue; // the ORDER BY must be EXACTLY the index columns (see the doc — tie-break)
        }
        let matches = order
            .iter()
            .enumerate()
            .all(|(i, (slot, descending, nulls_first, coll))| {
                if *descending || *nulls_first {
                    return false; // ASC + NULLS LAST only — the order a forward index walk realizes
                }
                if *slot != offset + idx.columns[i] {
                    return false; // the i-th index column, in key order
                }
                match key_collation_ctx(catalog, &table.columns[idx.columns[i]]) {
                    None => false, // Skewed / unresolvable — never walked for order (§12)
                    Some(None) => coll.is_none(),
                    Some(Some(c)) => matches!(coll, Some(c2) if c2.name == c.name),
                }
            });
        if matches {
            return Some(IndexOrder {
                name_key: idx.name.to_ascii_lowercase(),
                pk_width,
            });
        }
    }
    None
}

/// Detect a GIN-bounded scan over `columns`/`indexes` (gin.md §6): the lowest-named GIN index
/// whose array column at `offset + ci` has a GIN-accelerable conjunct (`col @> const`,
/// `col && const`, `const = ANY(col)`, or `col = const`). Factored out so the SELECT planner
/// (`detect_scan_bound`) and the UPDATE/DELETE scan both use the identical detection — the
/// mutations pass their own table's indexes/columns at `offset = 0`.
fn detect_gin_bound(
    filter: &RExpr,
    indexes: &[IndexDef],
    columns: &[Column],
    offset: usize,
) -> Option<GinBound> {
    for idx in indexes {
        if idx.kind != IndexKind::Gin {
            continue;
        }
        let ci = idx.columns[0];
        let col_global = offset + ci;
        let Some(elem_ty) = columns[ci].ty.array_element().map(|t| t.scalar()) else {
            continue; // a GIN column is always an array (the CREATE INDEX gate); defensive
        };
        if let Some((strategy, _)) = gin_match(filter, col_global) {
            return Some(GinBound {
                name_key: idx.name.to_ascii_lowercase(),
                elem_type: elem_ty,
                strategy,
                col_global,
            });
        }
    }
    None
}

/// Detect a GiST-bounded scan over `columns`/`indexes` (spec/design/gist.md §5): the lowest-named
/// GiST index whose range column at `offset + ci` has a GiST-accelerable conjunct (`col && const`
/// or `col @> const`). Factored out so the SELECT planner (`detect_scan_bound`) and the
/// UPDATE/DELETE scan share the identical detection (the GIN precedent) — the mutations pass their
/// own table's indexes/columns at `offset = 0`.
fn detect_gist_bound(
    filter: &RExpr,
    indexes: &[IndexDef],
    columns: &[Column],
    offset: usize,
) -> Option<GistBound> {
    for idx in indexes {
        if idx.kind != IndexKind::Gist {
            continue;
        }
        // The planner gather is single-operator: only a single-column GiST index accelerates a
        // `col && Q` / `col @> Q` / `col = Q` conjunct. A multi-column GiST index (an EXCLUDE
        // backing structure, gist.md §7) is probed only by the constraint, never the planner.
        if idx.columns.len() != 1 {
            continue;
        }
        let ci = idx.columns[0];
        let col_global = offset + ci;
        let col_ty = &columns[ci].ty;
        if col_ty.range_element().is_some() {
            // `range_ops` (GX1): a `col && Q` / `col @> Q` conjunct.
            if let Some((strategy, _)) = gist_match(filter, col_global) {
                return Some(GistBound {
                    name_key: idx.name.to_ascii_lowercase(),
                    strategy,
                    col_global,
                    scalar_type: None,
                });
            }
        } else if is_gist_scalar_type(col_ty) {
            // scalar `=` opclass (GX2): a `col = Q` conjunct over a fixed-width keyable scalar.
            if gist_scalar_match(filter, col_global).is_some() {
                return Some(GistBound {
                    name_key: idx.name.to_ascii_lowercase(),
                    strategy: crate::gist::GistStrategy::Equal,
                    col_global,
                    scalar_type: Some(col_ty.scalar()),
                });
            }
        }
    }
    None
}

/// Find the first WHERE AND-chain conjunct that a GiST `range_ops` index on `col_global`
/// accelerates (spec/design/gist.md §5): `col && Q` (overlap — symmetric, the column may be either
/// operand) or `col @> Q` (contains — asymmetric, the column must be the LEFT operand; `Q @> col`
/// is the non-accelerated `<@`, gist.md §5). `Q` must be a **constant** (re-evaluable per scan, not
/// per row). The other range operators (`<@`/`<<`/`>>`/`&<`/`&>`/`-|-`/`=`) stay full-scan this
/// slice (gist.md §5). Returns the descent strategy and a reference to the constant query operand —
/// used at plan time (the strategy) and exec time (recover the operand from `plan.filter`), so the
/// two agree on the same conjunct by construction.
fn gist_match(filter: &RExpr, col_global: usize) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    use crate::gist::GistStrategy;
    match filter {
        RExpr::And(l, r) => gist_match(l, col_global).or_else(|| gist_match(r, col_global)),
        // `col && Q` — overlap is symmetric in its operands.
        RExpr::RangeOp {
            op: RangeOp::Overlaps,
            args,
            ..
        } if args.len() == 2 => {
            if is_column(&args[0], col_global) && rexpr_is_constant(&args[1]) {
                Some((GistStrategy::Overlaps, &args[1]))
            } else if is_column(&args[1], col_global) && rexpr_is_constant(&args[0]) {
                Some((GistStrategy::Overlaps, &args[0]))
            } else {
                None
            }
        }
        // `col @> Q` — containment is asymmetric: the indexed column must be the container (LEFT).
        RExpr::RangeOp {
            op: RangeOp::Contains,
            args,
            ..
        } if args.len() == 2 => (is_column(&args[0], col_global) && rexpr_is_constant(&args[1]))
            .then_some((GistStrategy::Contains, &args[1])),
        _ => None,
    }
}

/// Find the first WHERE AND-chain conjunct that a GiST scalar `=` opclass on `col_global`
/// accelerates (spec/design/gist.md §6): `col = Q` where `Q` is a **constant** (re-evaluable per
/// scan, not per row). Equality is commutative — the column may be either operand. `<>` and the
/// inequalities are not accelerated (a GiST `=` opclass has only the equal strategy). Returns the
/// `Equal` strategy and a reference to the constant operand (recovered at exec from `plan.filter`,
/// so plan and exec agree on the same conjunct by construction — the `gist_match` precedent).
fn gist_scalar_match(
    filter: &RExpr,
    col_global: usize,
) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    use crate::gist::GistStrategy;
    match filter {
        RExpr::And(l, r) => {
            gist_scalar_match(l, col_global).or_else(|| gist_scalar_match(r, col_global))
        }
        RExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
            ..
        } => {
            if is_column(lhs, col_global) && rexpr_is_constant(rhs) {
                Some((GistStrategy::Equal, rhs.as_ref()))
            } else if is_column(rhs, col_global) && rexpr_is_constant(lhs) {
                Some((GistStrategy::Equal, lhs.as_ref()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Recover a GiST bound's constant query operand from the live filter at exec time — `gist_match`
/// for `range_ops` (`&&`/`@>`), `gist_scalar_match` for the scalar `=` opclass. Centralizes the
/// strategy dispatch so every scan site (SELECT / UPDATE / DELETE) recovers the operand uniformly.
fn gist_query_operand<'a>(filter: &'a RExpr, gb: &GistBound) -> Option<&'a RExpr> {
    match gb.strategy {
        crate::gist::GistStrategy::Equal => {
            gist_scalar_match(filter, gb.col_global).map(|(_, q)| q)
        }
        _ => gist_match(filter, gb.col_global).map(|(_, q)| q),
    }
}

/// Find the first WHERE AND-chain conjunct that a GIN index on `col_global` accelerates
/// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
/// (membership), or `col = Q` (exact array equality) where the query operand is a **constant**
/// (references no column / outer / subquery — re-evaluable per scan, not per row). `@>` is
/// asymmetric (the indexed column must be the LEFT operand — `Q @> col` is the non-accelerated
/// `<@`); `&&` and array `=` are symmetric (the column may be either operand). Returns the
/// strategy and a reference to the constant query operand. Used both at plan time (for the
/// strategy) and exec time (to recover the operand from `plan.filter`), so the two agree on the
/// same conjunct by construction.
fn gin_match(filter: &RExpr, col_global: usize) -> Option<(GinStrategy, &RExpr)> {
    match filter {
        RExpr::And(l, r) => gin_match(l, col_global).or_else(|| gin_match(r, col_global)),
        RExpr::ArrayFunc {
            func: ArrayFunc::Contains,
            args,
        } if args.len() == 2 => (is_column(&args[0], col_global) && rexpr_is_constant(&args[1]))
            .then_some((GinStrategy::Contains, &args[1])),
        RExpr::ArrayFunc {
            func: ArrayFunc::Overlaps,
            args,
        } if args.len() == 2 => {
            if is_column(&args[0], col_global) && rexpr_is_constant(&args[1]) {
                Some((GinStrategy::Overlaps, &args[1]))
            } else if is_column(&args[1], col_global) && rexpr_is_constant(&args[0]) {
                Some((GinStrategy::Overlaps, &args[0]))
            } else {
                None
            }
        }
        // `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either
        // operand, the constant array `Q` the other. Recovered query operand is `Q`; `gin_bound_rows`
        // reads it via `Equal` (the @>-superset gather + the residual `=`). `<>` is NOT matched
        // (only `CmpOp::Eq`). When the column is an array, the other constant operand is necessarily
        // an array too (resolve rejects an array/scalar `=`), so `Q` is always an array here.
        RExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
            ..
        } => {
            if is_column(lhs, col_global) && rexpr_is_constant(rhs) {
                Some((GinStrategy::Equal, rhs.as_ref()))
            } else if is_column(rhs, col_global) && rexpr_is_constant(lhs) {
                Some((GinStrategy::Equal, lhs.as_ref()))
            } else {
                None
            }
        }
        // `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be
        // ANY's ARRAY operand and `c` (the scalar `lhs`) a constant. Only `= ANY` (not `= ALL`,
        // not any other comparison/quantifier — those are not a single-term posting gather). The
        // recovered query operand is the scalar `c`; `gin_bound_rows` reads it via `Member`.
        RExpr::Quantified {
            op: CmpOp::Eq,
            all: false,
            lhs,
            array,
        } if is_column(array, col_global) && rexpr_is_constant(lhs) => {
            Some((GinStrategy::Member, lhs.as_ref()))
        }
        _ => None,
    }
}

/// Is `e` a reference to the column at global scope index `col_global`?
fn is_column(e: &RExpr, col_global: usize) -> bool {
    matches!(e, RExpr::Column(i) if *i == col_global)
}

/// Is `e` a **constant** expression — evaluable without a current/outer row (so its value is the
/// same for every scanned row, computable once)? False for any column, correlated outer column, or
/// subquery; true for literals, params, and pure operations over them. Used to admit a GIN query
/// operand `Q` (spec/design/gin.md §6: a constant query only this slice).
fn rexpr_is_constant(e: &RExpr) -> bool {
    match e {
        RExpr::Column(_) | RExpr::OuterColumn { .. } | RExpr::Subquery { .. } => false,
        RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstNull
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::Param(_) => true,
        RExpr::Row(xs) | RExpr::Array { elems: xs, .. } => xs.iter().all(rexpr_is_constant),
        RExpr::Field { base, .. } => rexpr_is_constant(base),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rexpr_is_constant(base)
                && subscripts
                    .iter()
                    .flat_map(subscript_bounds)
                    .all(rexpr_is_constant)
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => rexpr_is_constant(inner),
        RExpr::Neg { operand, .. } => rexpr_is_constant(operand),
        RExpr::Not(x) => rexpr_is_constant(x),
        RExpr::Casing { arg, .. } => rexpr_is_constant(arg),
        RExpr::AtTimeZone { zone, value, .. } => {
            rexpr_is_constant(zone) && rexpr_is_constant(value)
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rexpr_is_constant(unit)
                && rexpr_is_constant(value)
                && zone.as_ref().is_none_or(|z| rexpr_is_constant(z))
        }
        RExpr::Extract { value, .. } => rexpr_is_constant(value),
        RExpr::DateConvert { inner, .. } => rexpr_is_constant(inner),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. }
        | RExpr::And(lhs, rhs)
        | RExpr::Or(lhs, rhs) => rexpr_is_constant(lhs) && rexpr_is_constant(rhs),
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => rexpr_is_constant(base) && rexpr_is_constant(arg),
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rexpr_is_constant(a) && rexpr_is_constant(b)
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => rexpr_is_constant(operand),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .all(|(c, r)| rexpr_is_constant(c) && rexpr_is_constant(r))
                && rexpr_is_constant(els)
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::RangeFunc { args, .. }
        | RExpr::RegexFunc { args, .. }
        | RExpr::RangeCtor { args, .. }
        | RExpr::RangeOp { args, .. }
        | RExpr::RangeSetOp { args, .. }
        | RExpr::Variadic { args, .. }
        | RExpr::JsonBuild { args, .. }
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonPathFn { args, .. } => args.iter().all(rexpr_is_constant),
        RExpr::JsonSqlFn { ctx, path, .. } => rexpr_is_constant(ctx) && rexpr_is_constant(path),
        RExpr::InValues { lhs, .. } => rexpr_is_constant(lhs),
        RExpr::Quantified { lhs, array, .. } => rexpr_is_constant(lhs) && rexpr_is_constant(array),
    }
}

/// A secondary-index entry key (spec/design/indexes.md §3): each indexed column as the
/// encoding.md §2.2 nullable slot — `0x00` + the type's bare order-preserving key bytes when
/// present, the lone `0x01` for NULL (always tagged, even for a NOT NULL column) — then the
/// row's storage key as the suffix. The indexed value is always resident (never `Unfetched`):
/// a fixed-width type never spills, and a `text`/`bytea` value large enough to spill would
/// produce an over-`RECORD_MAX` entry key, rejected `0A000` at the insert that stored it — so
/// any value that actually reached the index is small enough to stay inline.
fn index_entry_key(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    def: &IndexDef,
    storage_key: &[u8],
    row: &Row,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for &ci in &def.columns {
        match &row[ci] {
            Value::Null => out.push(0x01),
            v => {
                // present tag, then the column type's order-preserving key (range-aware §2.11,
                // collated-text-aware §2.12)
                out.push(0x00);
                out.extend_from_slice(&encode_typed_key(&columns[ci].ty, v, colls[ci].as_deref())?);
            }
        }
    }
    out.extend_from_slice(storage_key);
    Ok(out)
}

/// The index entries a row contributes (spec/design/gin.md §4/§5): exactly one for an ordered
/// (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL element for a
/// GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index uniformly as "a
/// row maps to a set of entries." `colls` (column-ordinal-indexed) selects each text key column's
/// collated form (§2.12); GIN elements are fixed-width, so a GIN index never collates.
fn index_entry_keys(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    def: &IndexDef,
    storage_key: &[u8],
    row: &Row,
) -> Result<Vec<Vec<u8>>> {
    Ok(match def.kind {
        IndexKind::Btree => vec![index_entry_key(columns, colls, def, storage_key, row)?],
        IndexKind::Gin => gin_entries(columns, def, storage_key, row),
        IndexKind::Gist => gist_entries(columns, def, storage_key, row),
    })
}

/// A GiST index's entry keys for one row (spec/design/gist.md §4.1/§7): exactly one leaf key, the
/// per-column component bounds concatenated then `‖ storage_key` ([`gist::leaf_key_multi`]) — the
/// GIN `term ‖ skey` pattern, so all existing index maintenance (insert/update/delete) reuses it
/// unchanged. A single-column GX1/GX2 index has one component; an EXCLUDE backing index one per
/// `WITH` column. A NULL in **any** indexed column produces NO entry (the §7 exclusion NULL rule —
/// a row with a NULL excluded column never conflicts and is left out of the tree; the GIN NULL-skip
/// precedent). The empty range is a real value and IS indexed.
fn gist_entries(columns: &[Column], def: &IndexDef, storage_key: &[u8], row: &Row) -> Vec<Vec<u8>> {
    // Pre-encode scalar key bytes so the borrowed `GistLeafComp::Scalar(&[u8])` outlives the build.
    let mut scalar_keys: Vec<Vec<u8>> = Vec::new();
    for &ci in &def.columns {
        let col = &columns[ci];
        if matches!(row[ci], Value::Null) {
            return Vec::new(); // any NULL excluded column → row not indexed (NULL rule)
        }
        if col.ty.range_element().is_none() {
            // scalar `=` opclass: the value's order-preserving KEY bytes (gist.md §6). The column
            // is a FIXED-WIDTH keyable (the gate), so the key encoding is collation-free/infallible.
            let k = encode_key_value(col.ty.scalar(), &row[ci], None)
                .expect("a fixed-width GiST scalar key is infallible (no collation)");
            scalar_keys.push(k);
        }
    }
    let mut comps: Vec<crate::gist::GistLeafComp> = Vec::with_capacity(def.columns.len());
    let mut next_scalar = 0usize;
    for &ci in &def.columns {
        let col = &columns[ci];
        match col.ty.range_element() {
            Some(elem) => match &row[ci] {
                Value::Range(rv) => comps.push(crate::gist::GistLeafComp::Range(elem.scalar(), rv)),
                _ => unreachable!("a GiST range index column holds a range or NULL"),
            },
            None => {
                comps.push(crate::gist::GistLeafComp::Scalar(&scalar_keys[next_scalar]));
                next_scalar += 1;
            }
        }
    }
    vec![crate::gist::leaf_key_multi(&comps, storage_key)]
}

/// Build a row's `EXCLUDE` conjunction probe (spec/design/gist.md §7): one GiST query operand +
/// strategy per excluded column, in the backing index's column order. Returns `None` (the row is
/// **exempt**, never conflicts) when the **NULL rule** fires (any excluded column is NULL) or when a
/// `&&` element holds the **empty range** (`empty && anything` is FALSE, so the conjunction can
/// never be TRUE — this also sidesteps the empty-range overlap-descend trap, gist.md §5). The query
/// is fed to the resident GiST tree's `search`, whose leaf recheck IS the full conjunction, so a hit
/// is a genuine conflict.
fn exclusion_probe_query(
    columns: &[Column],
    exc: &ExclusionConstraint,
    row: &Row,
) -> Option<(Vec<crate::gist::GistQuery>, Vec<crate::gist::GistStrategy>)> {
    use crate::gist::{GistQuery, GistStrategy};
    let mut q = Vec::with_capacity(exc.elements.len());
    let mut strats = Vec::with_capacity(exc.elements.len());
    for el in &exc.elements {
        let ci = el.column;
        match (&row[ci], el.op) {
            (Value::Null, _) => return None, // NULL rule: exempt
            (Value::Range(rv), ExclusionOp::Overlaps) => {
                if rv.empty {
                    return None; // empty && anything is FALSE → exempt
                }
                q.push(GistQuery::Range(rv.clone()));
                strats.push(GistStrategy::Overlaps);
            }
            (v, ExclusionOp::Equal) => {
                let key = encode_key_value(columns[ci].ty.scalar(), v, None)
                    .expect("a fixed-width GiST scalar key is infallible (no collation)");
                q.push(GistQuery::Scalar(key));
                strats.push(GistStrategy::Equal);
            }
            _ => unreachable!("an && exclusion column holds a range or NULL"),
        }
    }
    Some((q, strats))
}

/// Does the `(expr_i op_i)` conjunction hold between two rows (spec/design/gist.md §7)? Used for the
/// in-batch new-row-vs-new-row check (the resident GiST tree holds only stored rows). A NULL in any
/// excluded column of either row, or an empty range under `&&` (`range_overlaps` of an empty range
/// is FALSE), makes that element not-TRUE → no conflict. Returns `true` only when EVERY element is
/// definitely TRUE.
fn exclusion_pair_conflicts(
    columns: &[Column],
    exc: &ExclusionConstraint,
    a: &Row,
    b: &Row,
) -> bool {
    for el in &exc.elements {
        let ci = el.column;
        let (va, vb) = (&a[ci], &b[ci]);
        if matches!(va, Value::Null) || matches!(vb, Value::Null) {
            return false;
        }
        let ok = match el.op {
            ExclusionOp::Overlaps => match (va, vb) {
                (Value::Range(ra), Value::Range(rb)) => crate::range::range_overlaps(ra, rb),
                _ => unreachable!("an && exclusion column holds a range or NULL"),
            },
            ExclusionOp::Equal => {
                let ka = encode_key_value(columns[ci].ty.scalar(), va, None)
                    .expect("a fixed-width GiST scalar key is infallible");
                let kb = encode_key_value(columns[ci].ty.scalar(), vb, None)
                    .expect("a fixed-width GiST scalar key is infallible");
                ka == kb
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Is `elem` an element type a GIN (`array_ops`) index admits? The integers, `boolean`, `uuid`,
/// `date`, `timestamp`, `timestamptz` (spec/design/gin.md §3): the GIN term IS the element's
/// order-preserving key encoding (§4) and a term carries no length/terminator framing, so only the
/// FIXED-WIDTH keyables qualify. The VARIABLE-width keyables (`text`, `bytea`, `decimal`) — though
/// valid ordered-index / PK keys — are 0A000 here, as is `float`. `interval` is fixed-width keyable
/// (its 16-byte span key landed this slice, encoding.md §2.10) but its GIN *element* support is a
/// separate follow-on slice (gin.md §3/§10 — like each element type before it), so it is not yet
/// admitted here.
fn is_gin_element_type(elem: &Type) -> bool {
    elem.is_integer()
        || elem.is_bool()
        || elem.is_uuid()
        || elem.is_timestamp()
        || elem.is_timestamptz()
        || elem.is_date()
}

/// Does the scalar `=` GiST opclass admit this column type (spec/design/gist.md §6)? The FIXED-WIDTH
/// keyables — integers, boolean, uuid, date, timestamp, timestamptz — whose bound is `[min, max]`
/// over the order-preserving key encoding, compared as raw bytes (no decode, no collation). Exactly
/// `is_gin_element_type`'s set (both stage on the fixed-width key-encodable scalars), kept a separate
/// predicate so the two surfaces evolve independently.
fn is_gist_scalar_type(ty: &Type) -> bool {
    ty.is_integer()
        || ty.is_bool()
        || ty.is_uuid()
        || ty.is_timestamp()
        || ty.is_timestamptz()
        || ty.is_date()
}

/// A keyable scalar the GiST scalar `=` opclass will eventually admit but defers this slice
/// (spec/design/gist.md §6/§11): the VARIABLE-width / collation-sensitive keyables — `text`,
/// `bytea`, `decimal`, `interval`. A column of one of these is `0A000` ("not supported yet"), not
/// `42704` (it is on the roadmap, like each GIN element type before it).
fn is_gist_deferred_scalar_type(ty: &Type) -> bool {
    ty.is_text() || ty.is_bytea() || ty.is_decimal() || ty.is_interval()
}

/// A GIN index's entry keys for one row (spec/design/gin.md §4): one entry per DISTINCT non-NULL
/// array element — `encode_element(term) ‖ storage_key`, with NO presence tag (a term is never
/// NULL) and an empty payload. A NULL array column value and an empty array both yield no entries
/// (so they never appear in any posting list — correct for `@>`/`&&`). Returned sorted by encoded
/// term (= key-encoding byte order, which is order-preserving for every admitted element type), so
/// the per-row order is deterministic. `array_ops` over any fixed-width key-encodable element type.
fn gin_entries(columns: &[Column], def: &IndexDef, storage_key: &[u8], row: &Row) -> Vec<Vec<u8>> {
    let ci = def.columns[0];
    let elem_ty = columns[ci]
        .ty
        .array_element()
        .expect("a GIN index column is an array (CREATE INDEX gate)")
        .scalar();
    let mut terms: Vec<Vec<u8>> = Vec::new();
    if let Value::Array(arr) = &row[ci] {
        for el in &arr.elements {
            // a NULL element contributes no term; a non-keyable element is impossible under the gate
            if !matches!(el, Value::Null) {
                // a GIN element is fixed-width (is_gin_element_type excludes text), so it never
                // collates and the key encoding is infallible.
                terms.push(
                    encode_key_value(elem_ty, el, None)
                        .expect("a GIN element key is infallible (fixed-width, no collation)"),
                );
            }
        }
    }
    // Dedup by the encoded term: the encoding is a bijection, so byte-dedup == value-dedup, and
    // byte-sort == value-sort (order-preserving). Each distinct term yields one entry.
    terms.sort_unstable();
    terms.dedup();
    terms
        .into_iter()
        .map(|mut entry| {
            entry.extend_from_slice(storage_key);
            entry
        })
        .collect()
}

/// A row's PRIMARY-KEY STORAGE KEY (spec/design/encoding.md §2.3): the concatenation of the
/// members' order-preserving encodings in key order. Every keyable type is self-delimiting (the
/// scalars fixed-width or `0x00`-terminated, a `range` container framed §2.11), so the
/// concatenation is self-delimiting and `memcmp` equals the tuple's logical order. Each member is
/// encoded by the shared range-aware [`encode_typed_key`] (so a range PK member recurses into the
/// element codec, encoding.md §2.11); the tuple carries each member's full `Type` for that reason.
/// Shared by the INSERT duplicate check and the ON CONFLICT arbiter probe (spec/design/upsert.md §3);
/// a PK column is NOT NULL, so there is no presence tag and no NULL arm. `float`/`composite`/`array`
/// PKs are rejected at CREATE TABLE, so those value kinds never reach here. `colls`
/// (column-ordinal-indexed) selects a text PK member's collated form (§2.12); a non-`C` collated
/// member can fail the sort-key build (`0A000`), propagated here.
fn encode_pk_key(
    pk: &[(usize, Type)],
    colls: &[Option<std::sync::Arc<Collation>>],
    row: &Row,
) -> Result<Vec<u8>> {
    let mut k = Vec::new();
    for (i, pk_ty) in pk {
        k.extend_from_slice(&encode_typed_key(pk_ty, &row[*i], colls[*i].as_deref())?);
    }
    Ok(k)
}

/// A row's UNIQUENESS PROBE KEY for one unique index (spec/design/indexes.md §8): the §3
/// entry key's slot prefix — without the storage-key suffix — or `None` when any component
/// is NULL (*NULLS DISTINCT*: such a tuple never conflicts). Two rows conflict iff they
/// yield the same `Some` prefix. `colls` (column-ordinal-indexed) selects each text column's
/// collated form (§2.12).
fn index_prefix_key(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    def: &IndexDef,
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    for &ci in &def.columns {
        match &row[ci] {
            Value::Null => return Ok(None),
            v => {
                // present tag, then the column type's order-preserving key (range-aware §2.11,
                // collated-text-aware §2.12)
                out.push(0x00);
                out.extend_from_slice(&encode_typed_key(&columns[ci].ty, v, colls[ci].as_deref())?);
            }
        }
    }
    Ok(Some(out))
}

/// The half-open byte range `[prefix, byte-successor(prefix))` — every index entry whose
/// slot prefix equals `prefix` (the suffix makes tree keys unique, so equal prefixes sit
/// adjacent). The uniqueness probes range over it (spec/design/indexes.md §8).
fn unique_probe_bound(prefix: &[u8]) -> KeyBound {
    KeyBound {
        lo: Some(prefix.to_vec()),
        lo_inc: true,
        hi: prefix_successor(prefix),
        hi_inc: false,
    }
}

/// The byte-successor of a prefix: the smallest byte string greater than every string that
/// extends `p`. Increment the last non-0xFF byte and truncate after it; an all-0xFF prefix
/// has no successor (`None` ⇒ unbounded high end).
fn prefix_successor(p: &[u8]) -> Option<Vec<u8>> {
    let mut s = p.to_vec();
    while let Some(last) = s.last_mut() {
        if *last == 0xFF {
            s.pop();
        } else {
            *last += 1;
            return Some(s);
        }
    }
    None
}

/// The order-preserving key bytes for one keyable value (encoding.md §2), matching the PK / index
/// encoders. `value` is non-NULL and of a keyable type (a foreign-key column always is — its type
/// equals a PK/UNIQUE parent column, CREATE TABLE §6.2). `coll` is the text component's frozen
/// collation: `None` (the fast path, and every non-text type) keys a `text` by its raw UTF-8
/// (`text-terminated-escape` §2.4); `Some(c)` keys it by the collation's UCA sort key
/// (`text-collated-sortkey` §2.12), which can fail (`0A000`) on a code point the collation does not
/// map — propagated, so a collated INSERT of an unmapped string aborts the write.
fn encode_key_value(ty: ScalarType, value: &Value, coll: Option<&Collation>) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Int(n) => encode_int(ty, *n),
        Value::Bool(b) => encode_bool(*b),
        Value::Uuid(u) => u.to_vec(),
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_int(ty, *m),
        Value::Date(d) => encode_int(ty, *d as i64),
        Value::Text(s) => match coll {
            Some(c) => collation::sort_key(c, s)?,
            None => encode_terminated(s.as_bytes()),
        },
        Value::Bytea(b) => encode_terminated(b),
        Value::Decimal(d) => d.encode_key(),
        Value::Interval(iv) => iv.encode_key(),
        Value::Float64(f) => encode_f64_key(*f),
        Value::Float32(f) => encode_f32_key(*f),
        _ => unreachable!("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)"),
    })
}

/// The `float-order-preserving` key body for an `f64` (encoding.md §2.8): canonicalize via
/// [`canon_f64_bits`] (`-0 → +0`, every NaN → one quiet pattern), take the bits big-endian, then
/// **if the sign bit is set flip all 64 bits, else flip just the sign bit** — mapping the binary64
/// total order (§3, `-Inf < finite < +Inf < NaN`) onto unsigned byte order. Fixed 8 bytes, so
/// self-delimiting by width (no escape/terminator). `-0`/`+0` and any two NaNs canonicalize to one
/// key, so a `UNIQUE` float key treats them as one. Infallible.
fn encode_f64_key(f: f64) -> Vec<u8> {
    let mut bits = crate::value::canon_f64_bits(f);
    bits ^= if bits >> 63 == 1 { u64::MAX } else { 1 << 63 };
    bits.to_be_bytes().to_vec()
}

/// As [`encode_f64_key`], for `f32` (binary32, 4 bytes — the `float-order-preserving` rule §2.8).
fn encode_f32_key(f: f32) -> Vec<u8> {
    let mut bits = crate::value::canon_f32_bits(f);
    bits ^= if bits >> 31 == 1 { u32::MAX } else { 1 << 31 };
    bits.to_be_bytes().to_vec()
}

/// The order-preserving key bytes for one keyable value given its column **`Type`** — the
/// range-aware encoder threaded through every key path (PK, index entry/prefix, FK probe). A range
/// recurses into the `range-bounds` container codec (encoding.md §2.11), pulling its element scalar
/// from the column type; every other keyable value ignores the wrapper and dispatches on its scalar
/// via [`encode_key_value`]. `value` is non-NULL (callers handle the NULL slot tag separately), and
/// a range column always holds a `Value::Range`, so the scalar arm never sees a range type. `coll`
/// selects a `text` column's key form (encoding.md §2.12); it never applies to a range element (no
/// range subtype is text).
fn encode_typed_key(ty: &Type, value: &Value, coll: Option<&Collation>) -> Result<Vec<u8>> {
    match value {
        Value::Range(rv) => {
            let elem = ty
                .range_element()
                .expect("a range key value has a range column type")
                .scalar();
            Ok(crate::range::encode_range_key(elem, rv))
        }
        Value::Array(a) => {
            let elem = ty
                .array_element()
                .expect("an array key value has an array column type");
            encode_array_key(elem, a)
        }
        _ => encode_key_value(ty.scalar(), value, coll),
    }
}

/// Whether `ty` is an **array** whose element is a key-encodable scalar — so the array is a valid
/// `PRIMARY KEY` / index / `UNIQUE` / FK key (encoding.md §2.14, the `array-elements-terminated` rule).
/// A `float`-element array (`f64[]`/`f32[]`) IS keyable (the §2.8 narrowing lifted — a float at rest is
/// in-contract); only a composite-element array (composite is not yet keyable) is NOT keyable, the same
/// narrowing the bare composite scalar key carries.
fn is_array_keyable(ty: &Type) -> bool {
    ty.array_element().is_some_and(is_keyable_scalar)
}

/// Whether `ty` is a key-encodable **scalar** — the element-type gate for [`is_array_keyable`].
/// Mirrors the inline scalar gate the PK/UNIQUE/index resolvers apply directly. With `float` keys
/// exercised (§2.8) every scalar is keyable, so this is the full keyable-scalar set; only the
/// recursive `composite` container is excluded (it has no value-kind here — a composite element
/// would arrive as `Type::Composite`, which none of these predicates match).
fn is_keyable_scalar(ty: &Type) -> bool {
    ty.is_integer()
        || ty.is_bool()
        || ty.is_text()
        || ty.is_bytea()
        || ty.is_decimal()
        || ty.is_uuid()
        || ty.is_timestamp()
        || ty.is_timestamptz()
        || ty.is_date()
        || ty.is_interval()
        || ty.is_float()
}

/// The order-preserving `array-elements-terminated` key for an array value (encoding.md §2.14) — the
/// engine's second container key, recursing into each element's own key. Reproduces the in-memory
/// `array_total_cmp` order (array.md §5) under `memcmp`: per flattened (row-major) element a marker
/// (`0x01` present ‖ the element key, `0x02` NULL) so present sorts before NULL and a shorter list
/// reaches the `0x00` terminator first; then the shape suffix (`ndim`, then per dimension a `u32` BE
/// length and the `i32` `int-be-signflip` lower bound) breaks ties among equal-element-prefix,
/// equal-count arrays. The element is a key-encodable **scalar** (`float` elements included since the
/// §2.8 lift; the DDL gate rejects only a composite element `0A000`), so the per-element key is
/// [`encode_key_value`]; an array element key uses the `C` byte order (a collated array-element key is
/// not a feature this slice).
fn encode_array_key(elem_ty: &Type, a: &ArrayVal) -> Result<Vec<u8>> {
    let elem = elem_ty.scalar();
    let mut out = Vec::new();
    for e in &a.elements {
        match e {
            Value::Null => out.push(0x02), // NULL element — sorts after every present element
            v => {
                out.push(0x01); // present element marker
                out.extend_from_slice(&encode_key_value(elem, v, None)?);
            }
        }
    }
    out.push(0x00); // terminator — a shorter element list sorts before a longer one
    out.push(a.ndim() as u8);
    for d in 0..a.ndim() {
        out.extend_from_slice(&(a.dims[d] as u32).to_be_bytes());
        out.extend_from_slice(&encode_int(ScalarType::Int32, a.lbounds[d] as i64));
    }
    Ok(out)
}

/// A built foreign-key probe (spec/design/constraints.md §6.4/§6.8): the bytes to look up in the
/// parent, tagged with which physical tree to probe.
pub(crate) enum FkProbe {
    /// The parent's PK storage key (bare member encodings concatenated, in PK key order).
    Pk(Vec<u8>),
    /// A parent unique index's prefix (0x00-tagged slots, in index-key order) + the lowercased
    /// index name.
    Unique { index: String, prefix: Vec<u8> },
}

impl FkProbe {
    /// The raw probe bytes — used to compare against this statement's batch end state (§6.4). Two
    /// probes of one FK share the same byte space (a given FK always probes the PK or always a
    /// fixed unique index), so byte equality is a valid set membership test.
    fn bytes(&self) -> &[u8] {
        match self {
            FkProbe::Pk(b) => b,
            FkProbe::Unique { prefix, .. } => prefix,
        }
    }
}

/// Build the parent-key probe for `fk` from `row`, taking each referenced parent column's value
/// from `row[ordinals[i]]` where `ordinals[i]` supplies `fk.ref_columns[i]`. So the child side
/// passes `ordinals = &fk.columns` (local columns), and a self-reference batch entry passes
/// `ordinals = &fk.ref_columns` (the row viewed as a parent). Returns `None` when any supplied
/// value is NULL (MATCH SIMPLE exempt — §6.3). The probe uses the parent's PK when the referenced
/// set is the PK, else the matching unique index (re-derived deterministically — §6.8).
fn fk_probe(
    fk: &ForeignKeyConstraint,
    parent: &Table,
    parent_colls: &[Option<std::sync::Arc<Collation>>],
    row: &Row,
    ordinals: &[usize],
) -> Result<Option<FkProbe>> {
    // MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
    if ordinals.iter().any(|&o| matches!(row[o], Value::Null)) {
        return Ok(None);
    }
    // The value supplying parent column `pcol` (the fk pairing: ref_columns[i] ⇄ ordinals[i]).
    let value_for = |pcol: usize| -> &Value {
        let i = fk
            .ref_columns
            .iter()
            .position(|&r| r == pcol)
            .expect("a parent key column is one of the FK's referenced columns");
        &row[ordinals[i]]
    };
    // The probe must match the PARENT's stored key, so a collated parent key column is encoded with
    // the PARENT's collation (encoding.md §2.12), independent of the child column's own collation.
    let ref_set = sorted_unique(&fk.ref_columns);
    if !parent.pk.is_empty() && sorted_unique(&parent.pk) == ref_set {
        let mut k = Vec::new();
        for &pcol in &parent.pk {
            k.extend_from_slice(&encode_typed_key(
                &parent.columns[pcol].ty,
                value_for(pcol),
                parent_colls[pcol].as_deref(),
            )?);
        }
        Ok(Some(FkProbe::Pk(k)))
    } else {
        let idx = parent
            .indexes
            .iter()
            .find(|i| i.unique && sorted_unique(&i.columns) == ref_set)
            .expect("referenced columns matched a unique key at CREATE TABLE §6.2");
        let mut prefix = Vec::new();
        for &pcol in &idx.columns {
            prefix.push(0x00);
            prefix.extend_from_slice(&encode_typed_key(
                &parent.columns[pcol].ty,
                value_for(pcol),
                parent_colls[pcol].as_deref(),
            )?);
        }
        Ok(Some(FkProbe::Unique {
            index: idx.name.to_ascii_lowercase(),
            prefix,
        }))
    }
}

/// Flatten the WHERE's top-level AND-chain (an OR is never descended — a disjunction is not a
/// contiguous range) and collect every `pk <cmp> const-source` conjunct. `None` ⇒ no usable bound
/// (full scan). Conservative + sound: an unrecognized conjunct contributes no bound and stays in the
/// residual filter.
fn detect_pk_bound(
    filter: &RExpr,
    pk_idx: usize,
    pk_type: ScalarType,
    coll: Option<std::sync::Arc<Collation>>,
) -> Option<PkBound> {
    let mut terms = Vec::new();
    collect_bound_terms(
        filter,
        pk_idx,
        pk_type,
        coll.as_ref().map(|c| c.name.as_str()),
        None,
        &mut terms,
    );
    if terms.is_empty() {
        None
    } else {
        Some(PkBound {
            pk_type,
            terms,
            coll,
        })
    }
}

/// `sibling_cutoff` (index-nested-loop join, cost.md §3 "JOIN"): when `Some(cut)`, a bare column
/// reference whose GLOBAL index is `< cut` — an EARLIER join relation's column — is a valid bound
/// source (`BoundSrc::Sibling`), resolved per outer row from the combined left-hand row. `None`
/// (the ordinary once-materialized bound) accepts only literals/params/outer references.
fn collect_bound_terms(
    e: &RExpr,
    pk_idx: usize,
    pk_type: ScalarType,
    col_coll: Option<&str>,
    sibling_cutoff: Option<usize>,
    terms: &mut Vec<BoundTerm>,
) {
    match e {
        RExpr::And(l, r) => {
            collect_bound_terms(l, pk_idx, pk_type, col_coll, sibling_cutoff, terms);
            collect_bound_terms(r, pk_idx, pk_type, col_coll, sibling_cutoff, terms);
        }
        // `<>` is not a contiguous range, so it never seeds an index/PK bound — it stays in the
        // residual filter (a full scan + filter). Skipping it here keeps the deterministic cost
        // identical to Go/TS, where `asBoundTerm` excludes it the same way.
        // A comparison bounds the key only when ITS resolved collation matches the key column's
        // frozen collation (`col_coll`) — so the comparison orders text the SAME way the B-tree is
        // keyed (spec/design/collation.md §8). `C` key ⇔ a `C`/byte comparison (both `None`); a
        // collated key ⇔ a comparison under the SAME collation (the column's implicit collation, or
        // an explicit `COLLATE "<that name>"`). A comparison under a DIFFERENT collation —
        // `name COLLATE "C"` over a `unicode` column, `COLLATE "de"` over `unicode` — does NOT
        // match: its order disagrees with the stored keys, so it stays a full scan + residual
        // filter. (A *skewed* collated key never reaches here — `key_collation_ctx` refuses the
        // whole bound, §12.) The probe is then encoded in the key column's form (sort key for a
        // collated `Full` column — `build_key_bound`/`index_bound_rows`).
        RExpr::Compare {
            op,
            lhs,
            rhs,
            collation,
        } if !matches!(op, CmpOp::Ne)
            && collation.as_ref().map(|c| c.name.as_str()) == col_coll =>
        {
            let is_pk = |x: &RExpr| matches!(x, RExpr::Column(i) if *i == pk_idx);
            // The PK on either side (op flipped when it is on the right); the other side a
            // matching-type const-source. Anything else contributes no term.
            let term = if is_pk(lhs) {
                const_source(rhs, pk_type, sibling_cutoff).map(|src| BoundTerm { op: *op, src })
            } else if is_pk(rhs) {
                const_source(lhs, pk_type, sibling_cutoff).map(|src| BoundTerm {
                    op: flip_cmp(*op),
                    src,
                })
            } else {
                None
            };
            if let Some(t) = term {
                terms.push(t);
            }
        }
        _ => {}
    }
}

/// Recognize a const-source operand whose static type matches the PK's storage type (a promoted
/// comparison — e.g. `intpk = 2.5` → a `ConstDecimal` — does not match, so it stays residual). A
/// bare correlated `OuterColumn` IS a const-source (its value is a runtime constant for a given
/// outer row); arithmetic etc. are not. A type-mismatched outer reference is wrapped in a `Cast` by
/// the resolver (as for the literal case above), so it never arrives here bare — the type check is
/// implicit and the match stays sound.
///
/// `sibling_cutoff` opens the index-nested-loop door (cost.md §3 "JOIN"): when `Some(cut)`, a bare
/// `Column(g)` whose GLOBAL index is `< cut` — a column of an EARLIER join relation — is a
/// `BoundSrc::Sibling`, resolved per outer row from the combined left-hand row. Like `OuterColumn`,
/// a bare sibling column implies a type match (a mismatch is a `Cast`, never bare — sound). A
/// same-relation or later-relation column is `>= cut`, so it stays residual (`None`).
fn const_source(e: &RExpr, pk_type: ScalarType, sibling_cutoff: Option<usize>) -> Option<BoundSrc> {
    match e {
        RExpr::Param(i) => Some(BoundSrc::Param(*i)),
        RExpr::ConstNull => Some(BoundSrc::Null),
        RExpr::ConstInt(n) if pk_type.is_integer() => Some(BoundSrc::Int(*n)),
        RExpr::ConstBool(b) if pk_type.is_bool() => Some(BoundSrc::Bool(*b)),
        RExpr::ConstUuid(u) if pk_type.is_uuid() => Some(BoundSrc::Uuid(*u)),
        RExpr::ConstTimestamp(m) if pk_type.is_timestamp() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstTimestamptz(m) if pk_type.is_timestamptz() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstDate(d) if pk_type.is_date() => Some(BoundSrc::Date(*d)),
        RExpr::ConstText(s) if pk_type.is_text() => Some(BoundSrc::Text(s.clone())),
        RExpr::ConstBytea(b) if pk_type.is_bytea() => Some(BoundSrc::Bytea(b.clone())),
        RExpr::ConstDecimal(d) if pk_type.is_decimal() => Some(BoundSrc::Decimal(d.clone())),
        RExpr::ConstInterval(iv) if pk_type.is_interval() => Some(BoundSrc::Interval(*iv)),
        RExpr::OuterColumn { level, index } => Some(BoundSrc::Outer {
            level: *level,
            index: *index,
        }),
        RExpr::Column(g) if sibling_cutoff.is_some_and(|cut| *g < cut) => {
            Some(BoundSrc::Sibling(*g))
        }
        _ => None,
    }
}

/// Swap a comparison's sense (for `const <op> pk` ⇒ `pk <flipped> const`). Eq and Ne are symmetric.
fn flip_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// Encode an OR / IN-list's equality const-sources into the key space and return a SORTED,
/// DE-DUPLICATED set of encoded keys — the distinct point probes a merged point-set bound visits
/// (cost.md §3 "OR / IN-list"). A src that is NULL (3VL-never-true) or does not encode into the key
/// domain (an out-of-range integer — no stored key can equal it) contributes no point and is skipped
/// (sound: the union stays a superset, and the residual filter re-checks each admitted row). Byte-
/// dedup == value-dedup and byte-sort == value-sort under the order-preserving key encoding
/// (encoding.md §2), so probing the sorted distinct keys yields rows in ascending key order with no
/// row visited twice. Shared by the PK and secondary-index point-set executors.
fn encode_key_set(
    key_type: ScalarType,
    srcs: &[BoundSrc],
    params: &[Value],
    outer: &[&[Value]],
    coll: Option<&Collation>,
    left: &[Value],
) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(srcs.len());
    for src in srcs {
        match encode_bound_key(key_type, src, params, outer, coll, left) {
            BoundKey::Null | BoundKey::OutOfRange => continue,
            BoundKey::Key(k) => keys.push(k),
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Build the concrete key range at exec time: encode each const-source and intersect the half-bounds.
/// `outer` carries the enclosing rows (innermost last) so a correlated `Outer` source resolves to
/// the current outer row's value; it is empty for a top-level statement. `left` is the current
/// combined left-hand row of a left-deep join, from which an index-nested-loop `Sibling` source
/// resolves (empty outside the join loop — a `Sibling` never appears there). `None` ⇒ the range
/// admits no key (a NULL const/value — 3VL — or contradictory bounds), so the scan reads nothing. An
/// out-of-range integer const drops only its own half-bound (a wider, still sound, scan).
fn build_key_bound(
    bp: &PkBound,
    params: &[Value],
    outer: &[&[Value]],
    left: &[Value],
) -> Option<KeyBound> {
    let mut b = KeyBound::unbounded();
    for t in &bp.terms {
        let key =
            match encode_bound_key(bp.pk_type, &t.src, params, outer, bp.coll.as_deref(), left) {
                BoundKey::Null => return None,
                BoundKey::OutOfRange => continue,
                BoundKey::Key(k) => k,
            };
        match t.op {
            CmpOp::Eq => {
                intersect_lo(&mut b, &key, true);
                intersect_hi(&mut b, &key, true);
            }
            CmpOp::Gt => intersect_lo(&mut b, &key, false),
            CmpOp::Ge => intersect_lo(&mut b, &key, true),
            CmpOp::Lt => intersect_hi(&mut b, &key, false),
            CmpOp::Le => intersect_hi(&mut b, &key, true),
            // `<>` never becomes a bound term (filtered in `collect_bound_terms`), so it never
            // reaches here; it contributes no half-bound regardless (sound — the residual filter
            // re-applies the whole WHERE).
            CmpOp::Ne => {}
        }
    }
    if bound_empty(&b) { None } else { Some(b) }
}

/// Turn an index access predicate into a concrete index-key range at exec time (indexes.md §5.1).
/// Encode the equality prefix into `p` (the concatenated present slots), then — if there is a range
/// column — start from `[P, P‖0x01)` (the upper endpoint stops before the range column's NULL slot,
/// since a range is never true for NULL) and intersect each range term; otherwise the range is
/// `[P, byte-successor(P))` (every entry extending `P`). `None` ⇒ the bound admits no key (a NULL /
/// disagreeing prefix equality, a NULL range endpoint, or a contradictory range). The returned
/// `usize` is `len(P)`, the byte count the row-key suffix skip advances past the equality-prefix
/// slots before width-skipping the remaining components.
fn build_index_bound(
    ib: &IndexBound,
    params: &[Value],
    outer: &[&[Value]],
    left: &[Value],
) -> Option<(KeyBound, usize)> {
    let mut p: Vec<u8> = Vec::new();
    for ec in &ib.eq_cols {
        // Every equality const-source on this column must encode to ONE agreed value: a NULL is
        // 3VL-never-true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
        // integer can equal no stored value — all provably empty.
        let mut agreed: Option<Vec<u8>> = None;
        for src in &ec.srcs {
            let k =
                match encode_bound_key(ec.col_type, src, params, outer, ec.coll.as_deref(), left) {
                    BoundKey::Null | BoundKey::OutOfRange => return None,
                    BoundKey::Key(k) => k,
                };
            match &agreed {
                None => agreed = Some(k),
                Some(prev) if *prev == k => {}
                Some(_) => return None,
            }
        }
        p.push(0x00);
        p.extend_from_slice(&agreed.expect("an equality column has at least one source"));
    }
    let Some(rng) = &ib.range else {
        // Pure equality prefix: [P, byte-successor(P)).
        let b = KeyBound {
            lo: Some(p.clone()),
            lo_inc: true,
            hi: prefix_successor(&p),
            hi_inc: false,
        };
        return if bound_empty(&b) {
            None
        } else {
            Some((b, p.len()))
        };
    };
    // Equality prefix P + a range on the next column. Base: [P, P‖0x01) — present values only (the
    // 0x01 NULL tag sorts after every 0x00 present slot at this position).
    let mut hi_null = p.clone();
    hi_null.push(0x01);
    let mut b = KeyBound {
        lo: Some(p.clone()),
        lo_inc: true,
        hi: Some(hi_null),
        hi_inc: false,
    };
    for t in &rng.terms {
        // The range column is fixed-width (indexes.md §5.1 eligibility), so it is never collated: the
        // probe encodes with a `None` collation.
        let key = match encode_bound_key(rng.col_type, &t.src, params, outer, None, left) {
            BoundKey::Null => return None,
            BoundKey::OutOfRange => continue, // drop this half-bound (a wider, still-sound scan)
            BoundKey::Key(k) => k,
        };
        // P ‖ 0x00 ‖ encode(v) — the range column's present slot appended to the prefix.
        let mut ps = p.clone();
        ps.push(0x00);
        ps.extend_from_slice(&key);
        match t.op {
            CmpOp::Ge => intersect_lo(&mut b, &ps, true),
            // `>` skips the whole `c = v` subtree: the smallest key after every `P‖0x00‖v‖*` entry.
            CmpOp::Gt => match prefix_successor(&ps) {
                Some(s) => intersect_lo(&mut b, &s, true),
                None => return None, // no key exceeds the max — empty (unreachable: ps starts 0x00)
            },
            CmpOp::Lt => intersect_hi(&mut b, &ps, false),
            CmpOp::Le => match prefix_successor(&ps) {
                Some(s) => intersect_hi(&mut b, &s, false),
                None => {} // everything ≤ max — keep the base hi (P‖0x01)
            },
            // `=` never reaches range terms (filtered into the equality prefix); `<>` never becomes a
            // bound term at all. Both contribute no half-bound.
            CmpOp::Eq | CmpOp::Ne => {}
        }
    }
    if bound_empty(&b) {
        None
    } else {
        Some((b, p.len()))
    }
}

/// Encode a const-source's value into the PK's storage key (the same codec INSERT uses — `encode_int`
/// for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte `bool-byte` for boolean).
/// `Param`/`Outer`/`Sibling` resolve to a runtime `Value` first (the param table / the enclosing
/// outer row / the current combined left-hand row) and then encode through the shared path.
fn encode_bound_key(
    pk_ty: ScalarType,
    src: &BoundSrc,
    params: &[Value],
    outer: &[&[Value]],
    coll: Option<&Collation>,
    left: &[Value],
) -> BoundKey {
    match src {
        BoundSrc::Null => BoundKey::Null,
        BoundSrc::Int(n) => {
            if pk_ty.in_range(*n) {
                BoundKey::Key(encode_int(pk_ty, *n))
            } else {
                BoundKey::OutOfRange
            }
        }
        BoundSrc::Bool(b) => BoundKey::Key(encode_bool(*b)),
        BoundSrc::Uuid(u) => BoundKey::Key(u.to_vec()),
        BoundSrc::Timestamp(m) => BoundKey::Key(encode_int(pk_ty, *m)),
        BoundSrc::Date(d) => BoundKey::Key(encode_int(pk_ty, *d as i64)),
        BoundSrc::Text(s) => encode_text_bound(s, coll),
        BoundSrc::Bytea(b) => BoundKey::Key(encode_terminated(b)),
        BoundSrc::Decimal(d) => BoundKey::Key(d.encode_key()),
        BoundSrc::Interval(iv) => BoundKey::Key(iv.encode_key()),
        BoundSrc::Param(i) => encode_value_key(pk_ty, &params[*i], coll),
        // A correlated reference: column `index` of the enclosing row `level` hops out — the same
        // indexing the evaluator uses for `RExpr::OuterColumn` (innermost outer row is last).
        BoundSrc::Outer { level, index } => {
            encode_value_key(pk_ty, &outer[outer.len() - level][*index], coll)
        }
        // Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
        // current combined left-hand row (cost.md §3 "JOIN"). The join loop always passes a `left`
        // wide enough (the running row spans columns `[0, rel.offset)`, and `Sibling` indices are
        // `< rel.offset`); a stray out-of-range index widens to a full scan rather than panic.
        BoundSrc::Sibling(index) => match left.get(*index) {
            Some(v) => encode_value_key(pk_ty, v, coll),
            None => BoundKey::OutOfRange,
        },
    }
}

/// Encode a `text` probe into a key bound: the raw `text-terminated-escape` bytes for a `C` key
/// (`coll == None`, the fast path, encoding.md §2.4), or the collation's UCA sort key
/// (`text-collated-sortkey`, §2.12) for a `Full`-collated key. A sort-key build that fails on an
/// unmapped code point (the `0A000` the write/compare path raises, collation.md §6) becomes
/// `OutOfRange` here: the probe matches no stored (always-mapped) key, so the term contributes no
/// bound and the scan widens to a full scan + residual filter — which reproduces the exact
/// non-pushdown answer (empty for `=`, since equality is byte-identity §7; the `0A000` for an
/// ordering compare iff any row is actually scanned). Deterministic and identical across cores.
fn encode_text_bound(s: &str, coll: Option<&Collation>) -> BoundKey {
    match coll {
        Some(c) => match collation::sort_key(c, s) {
            Ok(k) => BoundKey::Key(k),
            Err(_) => BoundKey::OutOfRange,
        },
        None => BoundKey::Key(encode_terminated(s.as_bytes())),
    }
}

/// Encode a runtime `Value` (a bound param or a resolved outer column) into the PK's storage key.
/// A NULL value makes the comparison 3VL-unknown (an empty range); a value of a kind no key can
/// hold (or an integer outside the PK width) drops its half-bound, widening — still sound. `coll`
/// selects a `text` value's key form (collated sort key vs raw bytes — `encode_text_bound`).
fn encode_value_key(pk_ty: ScalarType, v: &Value, coll: Option<&Collation>) -> BoundKey {
    match v {
        Value::Null => BoundKey::Null,
        Value::Bool(b) => BoundKey::Key(encode_bool(*b)),
        Value::Uuid(u) => BoundKey::Key(u.to_vec()),
        Value::Int(n) => {
            if pk_ty.in_range(*n) {
                BoundKey::Key(encode_int(pk_ty, *n))
            } else {
                BoundKey::OutOfRange
            }
        }
        Value::Timestamp(m) | Value::Timestamptz(m) => BoundKey::Key(encode_int(pk_ty, *m)),
        Value::Date(d) => BoundKey::Key(encode_int(pk_ty, *d as i64)),
        Value::Text(s) => encode_text_bound(s, coll),
        Value::Bytea(b) => BoundKey::Key(encode_terminated(b)),
        Value::Decimal(d) => BoundKey::Key(d.encode_key()),
        Value::Interval(iv) => BoundKey::Key(iv.encode_key()),
        _ => BoundKey::OutOfRange,
    }
}

/// Tighten `b`'s lower bound to the more restrictive of (current, key); at an equal key an exclusive
/// bound (`inc=false`) wins.
fn intersect_lo(b: &mut KeyBound, key: &[u8], inc: bool) {
    let replace = match &b.lo {
        None => true,
        Some(cur) => key > cur.as_slice() || (key == cur.as_slice() && !inc),
    };
    if replace {
        b.lo = Some(key.to_vec());
        b.lo_inc = inc;
    }
}

/// Tighten `b`'s upper bound to the more restrictive of (current, key); at an equal key an exclusive
/// bound wins.
fn intersect_hi(b: &mut KeyBound, key: &[u8], inc: bool) {
    let replace = match &b.hi {
        None => true,
        Some(cur) => key < cur.as_slice() || (key == cur.as_slice() && !inc),
    };
    if replace {
        b.hi = Some(key.to_vec());
        b.hi_inc = inc;
    }
}

/// Whether the bound admits no key: lo above hi, or lo == hi with a non-inclusive endpoint.
fn bound_empty(b: &KeyBound) -> bool {
    match (&b.lo, &b.hi) {
        (Some(lo), Some(hi)) => {
            use std::cmp::Ordering::{Equal, Greater};
            match lo.cmp(hi) {
                Greater => true,
                Equal => !(b.lo_inc && b.hi_inc),
                _ => false,
            }
        }
        _ => false,
    }
}

/// A resolved set operation (spec/design/grammar.md §25): both operands planned with the same
/// parent scope (so a correlated set-op subquery works), the unified output types, and the
/// trailing ORDER BY / LIMIT / OFFSET resolved by output column.
pub(crate) struct SetOpPlan {
    op: SetOpKind,
    all: bool,
    lhs: QueryPlan,
    rhs: QueryPlan,
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    /// (output slot, descending, nulls_first) — the trailing ORDER BY resolved by output name.
    order: Vec<crate::spill::SortKey>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// A pull-based row cursor (Volcano-style): `next` yields one row, `None` at end of stream. The
/// cost meter is threaded IN per call rather than stored as a field, so the source holds no
/// borrow of it and the one `&mut Meter` is charged down a single call path with no aliasing —
/// the discipline that keeps this mirror-able with the Go/TS cores (CLAUDE.md §2). This is the
/// seam the streaming + point-lookup work (TODO Phase 6) builds on; today only `ScanSource`
/// exists and feeds the existing materialize-then-join pipeline unchanged, so results and cost
/// are byte-identical.
///
/// Charges the page_read block (one per B-tree node — spec/design/cost.md §3 "page_read") once,
/// before the first row, then storage_row_read per row yielded: the same units in the same order
/// as the inline scan loop it replaced. `rows` is the in-key-order materialization (eager today,
/// via `iter_in_key_order`; a lazy leaf walk later) — the charge accounting is identical either
/// way because cost is the logical node/row count, not a physical leaf fetch (pager.md §5). The
/// block fires on the first `next` even for an empty table (node_count 0 ⇒ a no-op charge), so
/// the accrued total never moves. `next` returns `Result` so the later lazy walk's leaf-fault
/// error has a home; the eager form never errors.
pub(crate) struct ScanSource {
    rows: std::vec::IntoIter<Row>,
    node_count: i64,
    charged_block: bool,
}

impl ScanSource {
    fn new(rows: Vec<Row>, node_count: i64) -> Self {
        ScanSource {
            rows: rows.into_iter(),
            node_count,
            charged_block: false,
        }
    }

    fn next(&mut self, m: &mut Meter) -> Result<Option<Row>> {
        // Enforce the cost ceiling before pulling the next row (CLAUDE.md §13): a runaway scan
        // (or a JOIN/correlated re-scan built on this source) stops deterministically once
        // accrued cost reaches the limit. No-op when unlimited (spec/design/cost.md §6).
        m.guard()?;
        if !self.charged_block {
            m.charge(COSTS.page_read * self.node_count);
            self.charged_block = true;
        }
        match self.rows.next() {
            Some(row) => {
                m.charge(COSTS.storage_row_read);
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }
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
pub(crate) enum AggCtx {
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
        /// Parallel to `group_keys`: for each master grouping key, `Some((canonical AST, type))` if
        /// it is a general **expression** key (`GROUP BY a + b`, aggregates.md §15) — so a projection
        /// / HAVING / ORDER BY expression that structurally matches it resolves to that group's
        /// synthetic slot — or `None` for a plain **column** key (matched by the column path instead).
        group_key_exprs: Vec<Option<(Expr, ResolvedType)>>,
        specs: Vec<AggSpec>,
        /// One entry per `GROUPING(c1, …, ck)` call collected from the projection / HAVING — each is
        /// the list of master-grouping-column POSITIONS (indices into `group_keys`) of its arguments.
        /// The call resolves to the placeholder slot `GROUPING_GS_BASE + index`, rebased after
        /// resolution to its real trailing synthetic slot (spec/design/aggregates.md §12).
        grouping_specs: Vec<Vec<usize>>,
    },
    /// A non-aggregate WINDOW query's projection (spec/design/window.md §5.1). Bare columns
    /// resolve to the real input row (like Forbidden); a `FuncCall` carrying an `OVER` clause
    /// collects into `specs` and resolves to the synthetic slot `base + window_index`, where
    /// A window function carrying an `OVER` clause collects into `specs` and resolves to the
    /// placeholder slot `WINDOW_RESULT_BASE + w` (rebased to `input_width + window_keys.len() + w`
    /// after resolution, once the row layout is final — like `GroupedWindow`). A non-column PARTITION
    /// BY / ORDER BY key (`PARTITION BY a + b`) is collected into `window_keys` and resolved to the
    /// placeholder slot `WINDOW_KEY_BASE + k`, rebased the same way.
    Window {
        specs: Vec<WindowSpec>,
        window_keys: Vec<RExpr>,
    },
    /// A GROUPED query that ALSO has window functions (spec/design/window.md §2/§5.1). The
    /// projection resolves against the grouped synthetic row `[group_keys…, agg_results…,
    /// window_results…]`: a bare column → its group-key slot (`42803` otherwise), a bare aggregate
    /// → an agg slot (`group_keys.len() + agg index`), and an `OVER` call → a window result. A
    /// window function's ARGUMENTS resolve under the grouped scope too (a nested aggregate collects
    /// into `agg_specs`, a bare column must be a grouping key), so `sum(sum(x)) OVER ()` is legal;
    /// its PARTITION BY / ORDER BY column keys must be grouping columns. Because the real window
    /// slot (`group_keys.len() + agg_specs.len() + w`) is not known until EVERY aggregate has been
    /// collected (one may be nested in a later window argument or the HAVING clause), a window
    /// result is resolved to the PLACEHOLDER slot `WINDOW_RESULT_BASE + w` and rewritten to its real
    /// slot by `rebase_placeholder_cols` after resolution finishes.
    GroupedWindow {
        group_keys: Vec<usize>,
        /// Parallel to `group_keys` — see `Collect::group_key_exprs` (general-expression group keys,
        /// aggregates.md §15). A grouped+window query matches them the same way in its projection.
        group_key_exprs: Vec<Option<(Expr, ResolvedType)>>,
        agg_specs: Vec<AggSpec>,
        /// `GROUPING(...)` calls collected from the projection / HAVING when the query ALSO has window
        /// functions (GROUPING SETS + window, aggregates.md §21) — same as `Collect::grouping_specs`.
        grouping_specs: Vec<Vec<usize>>,
        window_specs: Vec<WindowSpec>,
        /// Materialized window-key expressions (a non-column PARTITION BY / ORDER BY key —
        /// `PARTITION BY g + 1`, or `ORDER BY sum(x) + 1`), resolved against the grouped row and
        /// collected at the placeholder slot `WINDOW_KEY_BASE + k`. A bare grouping column or a bare
        /// aggregate (`ORDER BY sum(x)`) resolves to its real grouped-row slot and is NOT materialized.
        window_keys: Vec<RExpr>,
    },
}

/// The placeholder base a window query's window results carry until `rebase_placeholder_cols` rewrites
/// them to `input_width + window_keys.len() + w` (spec/design/window.md §5.1). Far above any real
/// column/synthetic-slot count, and below 2³¹ so it is valid on a 32-bit `usize` (the wasm32 build)
/// as well as f64-exact in the TS core's `number`. Kept identical across the three cores.
pub(crate) const WINDOW_RESULT_BASE: usize = 1 << 28;

/// The placeholder base a materialized window-key expression (a non-column PARTITION BY / ORDER BY
/// key — `PARTITION BY a + b`) carries until the rebase pass rewrites it to its real synthetic slot
/// `input_width + k` (spec/design/window.md §5.1). Disjoint from `WINDOW_RESULT_BASE`'s range, and
/// below 2³¹ (32-bit-`usize` / wasm32 safe). A bare-column key is NOT materialized — it keeps its real row slot.
pub(crate) const WINDOW_KEY_BASE: usize = 1 << 29;

/// The placeholder base a `GROUPING(...)` call carries until the rebase pass rewrites it to its real
/// trailing synthetic slot `group_keys.len() + agg_specs.len() + grouping_index` (the GROUPING
/// results follow the master columns + aggregate results in the grouped row —
/// spec/design/aggregates.md §12). Disjoint from the window bases, below 2³¹ (32-bit-`usize` / wasm32 safe).
/// GROUPING is mutually exclusive with window functions, so its placeholders never coexist with the
/// window ones in a projection.
pub(crate) const GROUPING_GS_BASE: usize = 1 << 30;

/// The placeholder base a materialized `ORDER BY` **expression** key's sort slot carries until it is
/// rebased to its real trailing slot `final_row_width + k` (the materialized order values are appended
/// after the input / window / grouped columns — grammar.md §10). Used only in the `SortKey` slot field
/// (a different namespace from the `RExpr::Column` bases above), but kept disjoint and below 2³¹
/// (32-bit-`usize` / wasm32 safe) for the same reasons. A column / ordinal key keeps its real slot.
pub(crate) const ORDER_EXPR_BASE: usize = 1 << 27;

/// The maximum number of grouping sets a `GROUP BY` may expand to (`CUBE` of n columns alone is
/// 2ⁿ). Beyond this the statement is aborted `54001` (statement_too_complex) — jed's structural-
/// complexity gate (a deliberate divergence from PostgreSQL's per-construct "CUBE is limited to 12
/// elements" / 54011; jed bounds the total expansion instead). spec/design/aggregates.md §12.
pub(crate) const MAX_GROUPING_SETS: usize = 4096;

/// One resolved window function (spec/design/window.md §5.1): its plan, the resolved PARTITION BY
/// key column slots (flat input-row indices), and the resolved within-partition ORDER BY (sort
/// keys over the input row, PK tie-break applied by the stable sort over the PK-ordered scan).
pub(crate) struct WindowSpec {
    plan: WindowPlan,
    partition: Vec<usize>,
    order: Vec<crate::spill::SortKey>,
    /// Resolved function arguments (empty for the no-argument ranking functions; `ntile`'s bucket
    /// count; lag/lead's value/offset/default; the aggregate operand; first/last/nth_value's value
    /// + nth_value's position).
    args: Vec<RExpr>,
    /// The resolved explicit frame; `None` is the default frame (RANGE UNBOUNDED PRECEDING TO
    /// CURRENT ROW with an ORDER BY, the whole partition without — window.md §6).
    frame: Option<ResolvedFrame>,
    /// `agg(x) FILTER (WHERE cond) OVER (…)` — a per-frame-row boolean restricting which frame rows
    /// fold into the window aggregate (aggregates.md §20). `Some` only for an aggregate window
    /// function (a non-aggregate window function with `FILTER` is `0A000`). A `FILTER` disables the
    /// sliding-frame optimization (a filtered row can't be cleanly un-folded) — every frame re-folds.
    filter: Option<RExpr>,
}

/// A resolved window frame (spec/design/window.md §6). `ROWS` physical offsets, `GROUPS` peer-group
/// offsets (both integer counts), and `RANGE` value offsets over the single ordering key.
pub(crate) struct ResolvedFrame {
    mode: crate::ast::FrameMode,
    start: ResolvedBound,
    end: ResolvedBound,
    /// Frame exclusion (`EXCLUDE …` — window.md §6): rows dropped from `[lo, hi)` per current row.
    exclude: crate::ast::FrameExclusion,
}

/// A resolved frame boundary. `Preceding`/`Following` carry the offset as a value: `Value::Int(n)`
/// (the row/group count) for `ROWS`/`GROUPS`, and the numeric `Value` (`Int` over an integer key,
/// `Decimal` over a decimal key) added to / subtracted from the ordering key for `RANGE`.
pub(crate) enum ResolvedBound {
    UnboundedPreceding,
    Preceding(Value),
    CurrentRow,
    Following(Value),
    UnboundedFollowing,
}

/// The runtime plan for one window function (spec/design/window.md §4). S0: `row_number` only;
/// ranking / offset / aggregate-window / frame plans land in S1–S4.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPlan {
    /// `ROW_NUMBER()` — the 1-based sequence position within the partition (frame-insensitive).
    RowNumber,
    /// `RANK()` — 1 + the number of rows in earlier peer groups (ties share a rank, then a gap).
    Rank,
    /// `DENSE_RANK()` — 1 + the number of earlier peer groups (ties share a rank, no gap).
    DenseRank,
    /// `PERCENT_RANK()` — (rank − 1) / (N − 1), 0 when N = 1; decimal (divergence D2).
    PercentRank,
    /// `CUME_DIST()` — (# rows through the current peer group) / N; decimal (divergence D2).
    CumeDist,
    /// `NTILE(n)` — distribute the partition into n ranked buckets (larger first), numbered 1..n.
    /// Position-based (not peer-based); n ≤ 0 → 22014; NULL n → NULL for every row.
    Ntile,
    /// `LAG(v [,off [,def]])` / `LEAD(...)` — the value `off` positions back / forward in the
    /// partition; `def` (or NULL) when the offset leaves the partition. Frame-insensitive.
    Lag,
    Lead,
    /// An aggregate used as a window function (S3): `sum/count/min/max/avg(...) OVER (...)`, folded
    /// over the row's default frame (running with a window ORDER BY, whole-partition without) or an
    /// explicit frame (S4). Reuses the aggregate `Acc` kernels; the operand (if any) is `args[0]`.
    Agg(AggPlan),
    /// `FIRST_VALUE(v)` / `LAST_VALUE(v)` — the value of the frame's first / last row (S4). `args[0]`
    /// is the value expression; frame-sensitive.
    FirstValue,
    LastValue,
    /// `NTH_VALUE(v, n)` — the value of the frame's n-th row, NULL if the frame has < n rows (S4).
    /// `args[0]` is the value, `args[1]` the position; frame-sensitive.
    NthValue,
}

/// The runtime plan for one aggregate, fixed at resolve from the function + operand type
/// (the PG widening — spec/design/aggregates.md §3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggPlan {
    /// COUNT(*) — count every row (NULLs included).
    CountStar,
    /// COUNT(expr) — count non-NULL inputs.
    Count,
    /// SUM(i16|i32) — accumulate i64, result i64 (traps 22003 at the i64 bound).
    SumInt,
    /// SUM(i64|decimal) — accumulate decimal, result decimal (traps 22003 at the cap).
    SumDecimal,
    /// AVG — accumulate a decimal sum + i64 count; result sum/count (decimal), NULL if count 0.
    Avg,
    /// SUM(f32|f64) — the STREAMING scan-order running total (spec/design/float.md §7; fold order
    /// ledgered non-deterministic). Carries the width so the fold re-rounds at the input width.
    SumFloat(ScalarType),
    /// AVG(f32|f64) — SUM (streaming scan-order fold) / count, one final rounding at the input width.
    AvgFloat(ScalarType),
    Min,
    Max,
    /// json_agg / jsonb_agg (and the `_strict` variants) — aggregate the inputs' JSON images into a
    /// JSON array (json-sql-functions.md §4). `compact` selects the `json` (compact) vs `jsonb`
    /// (canonical) result render; `strict` skips a NULL input (else a NULL → JSON null).
    JsonAgg {
        compact: bool,
        strict: bool,
    },
    /// json_object_agg / jsonb_object_agg (and the `_unique` variants) — aggregate (key, value) pairs
    /// (a `Row` operand) into a JSON object (json-sql-functions.md §4). `json` selects the json
    /// (insertion order + dups + " : " spacing) vs jsonb (canonical, last-wins) render; `unique`
    /// errors `22030` on a duplicate key.
    JsonObjectAgg {
        json: bool,
        unique: bool,
    },
    /// `mode() WITHIN GROUP (ORDER BY x)` — the most frequent value (tie → first in sort order),
    /// result the input type (spec/design/aggregates.md §13). The direction + buffered values live
    /// on the `Acc`; this is just the kernel id (kept f64-free so AggPlan stays `Copy`/`Eq`).
    OrderedSetMode,
    /// `percentile_disc(f) WITHIN GROUP (ORDER BY x)` — the discrete percentile, an actual input
    /// value at row `ceil(f·N)`; result the input type. Direction + fraction live on the `Acc`.
    OrderedSetDisc,
    /// `percentile_cont(f) WITHIN GROUP (ORDER BY x)` — the continuous (interpolated) percentile;
    /// numeric input widened to f64, result f64. Direction + fraction live on the `Acc`.
    OrderedSetCont,
    /// `percentile_cont(f) WITHIN GROUP (ORDER BY x)` over an **interval** input — the continuous
    /// percentile interpolated in the interval domain (`lo + (hi-lo)·pct`, PG `interval_lerp`);
    /// result `interval` (spec/design/aggregates.md §13). Values buffered as `Value::Interval`.
    OrderedSetContInterval,
    /// `rank(args) WITHIN GROUP (ORDER BY keys)` — the **hypothetical-set** rank: 1 + the number of
    /// group rows that sort strictly before the hypothetical row `args` (result `i64`, §19).
    HypoRank,
    /// `dense_rank(args) WITHIN GROUP (ORDER BY keys)` — 1 + the number of DISTINCT group values that
    /// sort strictly before the hypothetical row (result `i64`, §19).
    HypoDenseRank,
    /// `percent_rank(args) WITHIN GROUP (ORDER BY keys)` — `(rank − 1) / N` (result `f64`, §19).
    HypoPercentRank,
    /// `cume_dist(args) WITHIN GROUP (ORDER BY keys)` — `(#rows ≤ hyp + 1) / (N + 1)` (`f64`, §19).
    HypoCumeDist,
}

/// The resolve-time parameters of an ordered-set aggregate (spec/design/aggregates.md §13), kept
/// off `AggPlan` (which is `Copy`/`Eq`). `desc` is the `WITHIN GROUP` sort direction; `frac` is the
/// resolved **direct argument** (the percentile fraction) — resolved in the grouped context so it
/// references grouping columns by their synthetic key slots (a non-grouped column is `42803`,
/// matching PG's *"direct arguments … must use only grouped columns"*) and is evaluated **per group**
/// at finalize against the synthetic row. `None` for `mode` (no direct argument).
pub(crate) struct OsaParams {
    desc: bool,
    frac: Option<RExpr>,
    /// The `WITHIN GROUP` key's collation — `Some` for an explicit `COLLATE` or a column's frozen
    /// non-`C` collation; `None` for the default byte (`C`) order (aggregates.md §13). The finalize
    /// sort applies it to the buffered text values.
    collation: Option<std::sync::Arc<Collation>>,
}

/// One resolved aggregate: its plan and its resolved argument expression (evaluated per
/// input row against the real row). `operand` is `None` for COUNT(*). `distinct` (`COUNT(DISTINCT
/// x)`, aggregates.md §5) folds only the distinct non-NULL argument values — the fold loop keeps a
/// per-group value-canonical set and skips a value already seen. `filter` (`SUM(x) FILTER (WHERE
/// cond)`, aggregates.md §11) is a resolved boolean predicate evaluated per input row; only rows
/// for which it is TRUE are folded (so the filter applies before the DISTINCT dedup). Both are
/// only set in the aggregation stage; a window aggregate is never DISTINCT or FILTERed (0A000,
/// rejected at resolve).
pub(crate) struct AggSpec {
    plan: AggPlan,
    operand: Option<RExpr>,
    distinct: bool,
    filter: Option<RExpr>,
    /// `Some` for an ordered-set aggregate (`mode`/`percentile_*` — aggregates.md §13): the
    /// `WITHIN GROUP` sort direction + the constant fraction. `None` for every ordinary aggregate.
    osa: Option<OsaParams>,
    /// `Some` for a hypothetical-set aggregate (`rank`/`dense_rank`/`percent_rank`/`cume_dist`
    /// `WITHIN GROUP` — aggregates.md §19): the hypothetical-row direct args + the `WITHIN GROUP`
    /// key operands + per-key sort specs. `None` otherwise. (`operand` is `None` here — the keys
    /// are buffered as a tuple per row from `hypo.keys`.)
    hypo: Option<HypoParams>,
}

/// A single `WITHIN GROUP` ordering-key sort spec (aggregates.md §13/§19): direction, NULL
/// placement, and optional collation (text keys only).
pub(crate) struct KeySort {
    desc: bool,
    nulls_first: bool,
    collation: Option<std::sync::Arc<Collation>>,
}

/// The resolve-time parameters of a hypothetical-set aggregate (aggregates.md §19). `args` are the
/// hypothetical-row direct arguments (evaluated **per group** at finalize, like an OSA fraction —
/// they may reference grouping columns); `keys` are the `WITHIN GROUP` key operands (evaluated **per
/// row** during the fold and buffered as a tuple); `sorts` is the per-key ordering spec. The three
/// vectors have equal length (the arity check at resolve).
pub(crate) struct HypoParams {
    args: Vec<RExpr>,
    keys: Vec<RExpr>,
    sorts: Vec<KeySort>,
}

/// A running aggregate accumulator (one per AggSpec), folded per input row then finalized.
/// `Clone` so the window stage can snapshot a running accumulator at each peer-group boundary
/// (a running aggregate window's default frame — spec/design/window.md §6) without consuming it.
#[derive(Clone)]
pub(crate) enum Acc {
    CountStar(i64),
    Count(i64),
    SumInt {
        sum: i64,
        seen: bool,
    },
    SumDecimal {
        sum: Decimal,
        seen: bool,
    },
    Avg {
        sum: Decimal,
        count: i64,
    },
    /// Float SUM/AVG: a STREAMING scan-order running total of the finite inputs (float.md §7), with
    /// NaN / ±Inf presence tracked so the special-value resolution stays order-independent. The fold
    /// ORDER is ledgered non-deterministic (determinism_exceptions.toml `float-sum-order`) — O(1)
    /// memory, no buffer/sort. `is_avg` selects the final SUM vs SUM/count; `width` re-rounds `total`
    /// to binary32 each add when f32. `count` is the non-NULL count.
    FloatFold {
        width: ScalarType,
        is_avg: bool,
        total: f64,
        count: i64,
        any_nan: bool,
        pos_inf: bool,
        neg_inf: bool,
    },
    MinMax {
        cur: Option<Value>,
        is_min: bool,
    },
    /// json_agg / jsonb_agg accumulator (B4): the inputs' JSON-image nodes in row order. `compact`
    /// selects the json vs jsonb finalize type; `strict` skips NULL inputs. `seen` records whether the
    /// group had ANY input row: a zero-row group → SQL NULL, but a non-empty group all of whose rows
    /// the strict filter dropped → an empty array `[]` (PG distinguishes the two).
    JsonAgg {
        nodes: Vec<JsonNode>,
        compact: bool,
        strict: bool,
        seen: bool,
    },
    /// json_object_agg / jsonb_object_agg accumulator (B4): the (key, value) pairs in row order.
    /// `json` selects the json vs jsonb finalize render; `unique` errors `22030` on a duplicate key.
    /// `seen` distinguishes a zero-row group (→ NULL) from a non-empty one (→ an object, maybe `{}`).
    JsonObjectAgg {
        pairs: Vec<(String, Value)>,
        json: bool,
        unique: bool,
        seen: bool,
    },
    /// An ordered-set aggregate (`mode`/`percentile_disc`/`percentile_cont` — aggregates.md §13):
    /// buffer every non-NULL operand value, then sort + compute at finalize. `kind` selects the
    /// computation, `desc` the `WITHIN GROUP` direction. `frac` is the **evaluated** percentile
    /// fraction for this group (the direct argument is evaluated per group against the synthetic row
    /// just before finalize — aggregates.md §13): `Some(Value)` for `percentile_*` (the value may be
    /// `Value::Null` → NULL result, or an array → one percentile per element), `None` for `mode`. For
    /// `percentile_cont` the inputs are widened to f64 into `floats`; `mode`/`percentile_disc` buffer
    /// the original `Value`s into `vals`.
    OrderedSet {
        kind: AggPlan,
        desc: bool,
        frac: Option<Value>,
        /// The `WITHIN GROUP` key collation (aggregates.md §13) applied to the finalize sort of the
        /// buffered text values; `None` is the default byte (`C`) order.
        collation: Option<std::sync::Arc<Collation>>,
        vals: Vec<Value>,
        floats: Vec<f64>,
    },
    /// A hypothetical-set aggregate (`rank`/`dense_rank`/`percent_rank`/`cume_dist` — aggregates.md
    /// §19): buffer every row's `WITHIN GROUP` key tuple; at finalize (in the group-emission loop,
    /// where the per-group hypothetical row + the spec's sort specs are available) count how that
    /// hypothetical row would rank. `kind` selects the result formula.
    Hypothetical {
        kind: AggPlan,
        rows: Vec<Vec<Value>>,
    },
}

/// Compute an ordered-set aggregate's value over its collected group (spec/design/aggregates.md
/// §13). `mode` returns the most frequent value (tie → first in `WITHIN GROUP` sort order);
/// `percentile_disc` an actual value at row `ceil(p·N)`; `percentile_cont` the interpolated f64.
/// The fraction range check (`22003`) fires here, after the NULL-fraction check and before the
/// empty-group check — matching PG.
fn finalize_ordered_set(
    kind: AggPlan,
    desc: bool,
    collation: Option<&Collation>,
    frac: Option<&Value>,
    mut vals: Vec<Value>,
    mut floats: Vec<f64>,
) -> Result<Value> {
    match kind {
        AggPlan::OrderedSetMode => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // Sort by the WITHIN GROUP order (honoring the key's collation), then take the first
            // value of the longest run of equal values — the most frequent, ties broken by sort
            // order (the first such run). Run equality is value-canonical (byte equality), so the
            // collation affects only which tied value comes first.
            sort_osa_vals(&mut vals, collation, desc)?;
            let mut best_idx = 0usize;
            let mut best_count = 1usize;
            let mut run_start = 0usize;
            for i in 1..vals.len() {
                if value_cmp(&vals[i], &vals[run_start]) == std::cmp::Ordering::Equal {
                    let run_len = i - run_start + 1;
                    if run_len > best_count {
                        best_count = run_len;
                        best_idx = run_start;
                    }
                } else {
                    run_start = i;
                }
            }
            Ok(vals.swap_remove(best_idx))
        }
        AggPlan::OrderedSetDisc => {
            // percentile_disc: an actual sorted value at row ceil(p·N). The fraction may be a scalar
            // or an array (aggregates.md §18); `finalize_percentile` dispatches and applies the
            // NULL / range-check / empty rules per PG, computing each percentile over the sorted vals.
            sort_osa_vals(&mut vals, collation, desc)?;
            finalize_percentile(frac, vals.is_empty(), |p| Ok(percentile_disc_at(&vals, p)))
        }
        AggPlan::OrderedSetCont => {
            floats.sort_by(|a, b| dir_cmp(crate::value::total_cmp_f64(*a, *b), desc));
            finalize_percentile(frac, floats.is_empty(), |p| {
                Ok(Value::Float64(percentile_cont_at(&floats, p)))
            })
        }
        AggPlan::OrderedSetContInterval => {
            // percentile_cont over interval input: interpolate in the interval domain (PG
            // `interval_lerp` — aggregates.md §13). Values are sorted by their canonical span
            // (interval has no collation, so `sort_osa_vals` uses the value order).
            sort_osa_vals(&mut vals, collation, desc)?;
            finalize_percentile(frac, vals.is_empty(), |p| {
                let n = vals.len();
                let pos = p * ((n - 1) as f64);
                let first = pos.floor() as usize;
                let second = pos.ceil() as usize;
                let lo = expect_interval(&vals[first]);
                if first == second {
                    return Ok(Value::Interval(lo));
                }
                let hi = expect_interval(&vals[second]);
                Ok(Value::Interval(interval_lerp(lo, hi, pos - first as f64)?))
            })
        }
        _ => unreachable!("finalize_ordered_set is only called for the ordered-set plans"),
    }
}

/// Apply the percentile fraction (scalar or array) to a sorted group, computing each percentile via
/// `compute` (spec/design/aggregates.md §13/§18). PG's check order is preserved: a **scalar** NULL
/// fraction → NULL; otherwise the range check (`22003`) fires per fraction **before** the empty-group
/// check; an empty/all-NULL group → NULL (the whole result, even for an array). For an **array**
/// fraction the result is an array with one percentile per element (a NULL element → a NULL element),
/// after every non-NULL element has passed the range check.
fn finalize_percentile(
    frac: Option<&Value>,
    empty: bool,
    compute: impl Fn(f64) -> Result<Value>,
) -> Result<Value> {
    match frac {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::Array(arr)) => {
            // Range-check every non-NULL element FIRST (before the empty-group check, PG).
            let mut fracs: Vec<Option<f64>> = Vec::with_capacity(arr.elements.len());
            for el in &arr.elements {
                let pf = fraction_to_f64(Some(el))?;
                if let Some(p) = pf {
                    check_percentile_fraction(p)?;
                }
                fracs.push(pf);
            }
            if empty {
                return Ok(Value::Null); // an empty/all-NULL group → NULL (not an array of NULLs), PG
            }
            let mut out = Vec::with_capacity(fracs.len());
            for pf in fracs {
                out.push(match pf {
                    Some(p) => compute(p)?,
                    None => Value::Null,
                });
            }
            let n = out.len();
            Ok(Value::Array(crate::value::ArrayVal {
                dims: vec![n],
                lbounds: vec![1],
                elements: out,
            }))
        }
        Some(scalar) => {
            let Some(p) = fraction_to_f64(Some(scalar))? else {
                return Ok(Value::Null);
            };
            check_percentile_fraction(p)?;
            if empty {
                return Ok(Value::Null);
            }
            compute(p)
        }
    }
}

/// The `Interval` of a buffered `Value::Interval` (an `OrderedSetContInterval` group only ever
/// buffers intervals — the resolver gates the operand to `interval`).
fn expect_interval(v: &Value) -> crate::interval::Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("percentile_cont(interval) buffered a non-interval: {other:?}"),
    }
}

/// `interval_lerp(lo, hi, pct)` = `lo + (hi - lo)·pct`, PG's `orderedsetaggs.c` interval
/// interpolation (spec/design/aggregates.md §13). `interval_mul` below replicates PG's exact
/// field-cascade + rounding so the result is byte-identical to PostgreSQL.
fn interval_lerp(
    lo: crate::interval::Interval,
    hi: crate::interval::Interval,
    pct: f64,
) -> Result<crate::interval::Interval> {
    let diff = hi.sub(&lo)?;
    let scaled = interval_mul(diff, pct)?;
    scaled.add(&lo)
}

/// `interval * f64`, byte-identical to PostgreSQL's `interval_mul` (timestamp.c): multiply each
/// field by the factor, then cascade the fractional month/day parts down to days/micros with PG's
/// `TSROUND` (round to microsecond precision) and the `30 days/month`, `86400 s/day` conversions.
/// The operand is finite (no infinite intervals here) and the factor is a finite fraction in [0,1].
fn interval_mul(span: crate::interval::Interval, factor: f64) -> Result<crate::interval::Interval> {
    const DAYS_PER_MONTH: f64 = 30.0;
    const SECS_PER_DAY: f64 = 86400.0;
    const USECS_PER_SEC: f64 = 1_000_000.0;
    // TSROUND: round to microsecond precision (PG TS_PREC_INV = 1e6). PG uses `rint` — round to
    // nearest, ties to EVEN — so the result is byte-identical to PostgreSQL (not half-away-from-zero).
    let tsround = |j: f64| -> f64 { (j * USECS_PER_SEC).round_ties_even() / USECS_PER_SEC };
    let oor = || EngineError::new(SqlState::DatetimeFieldOverflow, "interval out of range");
    let fits_i32 = |x: f64| x >= i32::MIN as f64 && x < -(i32::MIN as f64);
    let fits_i64 = |x: f64| x >= i64::MIN as f64 && x < -(i64::MIN as f64);

    let orig_month = span.months;
    let orig_day = span.days;

    let result_double = span.months as f64 * factor;
    if result_double.is_nan() || !fits_i32(result_double) {
        return Err(oor());
    }
    let result_month = result_double as i32;

    let result_double = span.days as f64 * factor;
    if result_double.is_nan() || !fits_i32(result_double) {
        return Err(oor());
    }
    let mut result_day = result_double as i32;

    // Cascade fractional months → days, fractional days → micros (PG's exact sequence).
    let month_remainder_days =
        tsround((orig_month as f64 * factor - result_month as f64) * DAYS_PER_MONTH);
    let mut sec_remainder = tsround(
        (orig_day as f64 * factor - result_day as f64 + month_remainder_days
            - month_remainder_days as i64 as f64)
            * SECS_PER_DAY,
    );
    // Might exceed a day from rounding / cascade — push whole days up.
    if sec_remainder.abs() >= SECS_PER_DAY {
        result_day = result_day
            .checked_add((sec_remainder / SECS_PER_DAY) as i32)
            .ok_or_else(oor)?;
        sec_remainder -= (sec_remainder / SECS_PER_DAY) as i64 as f64 * SECS_PER_DAY;
    }
    result_day = result_day
        .checked_add(month_remainder_days as i32)
        .ok_or_else(oor)?;
    let result_double =
        (span.micros as f64 * factor + sec_remainder * USECS_PER_SEC).round_ties_even();
    if result_double.is_nan() || !fits_i64(result_double) {
        return Err(oor());
    }
    Ok(crate::interval::Interval {
        months: result_month,
        days: result_day,
        micros: result_double as i64,
    })
}

/// Compute a hypothetical-set aggregate's value (aggregates.md §19): given the buffered group key
/// tuples `rows`, the per-group hypothetical row `hyp`, and the `WITHIN GROUP` per-key sort specs,
/// count where `hyp` would rank. `rank` = 1 + rows strictly before `hyp`; `dense_rank` = 1 + distinct
/// values strictly before; `percent_rank` = `(rank-1)/N`; `cume_dist` = `(#rows ≤ hyp + 1)/(N+1)` —
/// PG's `orderedsetaggs.c` formulas exactly. Over an empty group: rank/dense_rank 1, percent_rank 0,
/// cume_dist 1.
fn finalize_hypothetical(
    kind: AggPlan,
    rows: &[Vec<Value>],
    hyp: &[Value],
    sorts: &[KeySort],
) -> Result<Value> {
    use std::cmp::Ordering;
    let n = rows.len();
    if n == 0 {
        return Ok(match kind {
            AggPlan::HypoRank | AggPlan::HypoDenseRank => Value::Int(1),
            AggPlan::HypoPercentRank => Value::Float64(0.0),
            AggPlan::HypoCumeDist => Value::Float64(1.0),
            _ => unreachable!("finalize_hypothetical only for the hypothetical-set plans"),
        });
    }
    let mut strictly_before = 0i64;
    let mut le = 0i64; // rows that sort ≤ hyp (for cume_dist's rank with flag +1)
    // The distinct strictly-before key tuples (for dense_rank), value-canonical (the group-key Eq).
    let mut distinct: HashSet<&Vec<Value>> = HashSet::new();
    for r in rows {
        match hypo_cmp(r, hyp, sorts)? {
            Ordering::Less => {
                strictly_before += 1;
                le += 1;
                distinct.insert(r);
            }
            Ordering::Equal => le += 1,
            Ordering::Greater => {}
        }
    }
    Ok(match kind {
        AggPlan::HypoRank => Value::Int(strictly_before + 1),
        AggPlan::HypoDenseRank => Value::Int(distinct.len() as i64 + 1),
        AggPlan::HypoPercentRank => Value::Float64(strictly_before as f64 / n as f64),
        AggPlan::HypoCumeDist => Value::Float64((le + 1) as f64 / (n + 1) as f64),
        _ => unreachable!("finalize_hypothetical only for the hypothetical-set plans"),
    })
}

/// Compare a buffered key tuple `a` to the hypothetical row `b` by the `WITHIN GROUP` order
/// (aggregates.md §19): the first key whose comparison is non-equal decides. Each key honors its
/// NULL placement, direction, and collation (a collated text key can fail `0A000`).
fn hypo_cmp(a: &[Value], b: &[Value], sorts: &[KeySort]) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    for (i, ks) in sorts.iter().enumerate() {
        let ord = compare_hypo_key(&a[i], &b[i], ks)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// Compare one `WITHIN GROUP` key pair under its sort spec (NULL placement + direction + collation),
/// mirroring `key_cmp` plus the collated-text path (aggregates.md §19).
fn compare_hypo_key(a: &Value, b: &Value, ks: &KeySort) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    Ok(match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if ks.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if ks.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let base = match (&ks.collation, a, b) {
                (Some(c), Value::Text(x), Value::Text(y)) => collated_cmp(c, x, y)?,
                _ => value_cmp(a, b),
            };
            if ks.desc { base.reverse() } else { base }
        }
    })
}

/// Convert an evaluated percentile fraction (the direct argument, evaluated per group) to `f64`
/// (aggregates.md §13/§17). `None` / `Value::Null` → `None` (a NULL fraction yields NULL). A numeric
/// value (the resolver restricts the fraction to a numeric family) widens via the IEEE / correctly-
/// rounded decimal cast. The range check (`22003`) is applied by the caller after this.
fn fraction_to_f64(frac: Option<&Value>) -> Result<Option<f64>> {
    Ok(match frac {
        None | Some(Value::Null) => None,
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Float32(f)) => Some(*f as f64),
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Decimal(d)) => match decimal_to_float(d, ScalarType::Float64)? {
            Value::Float64(f) => Some(f),
            _ => unreachable!("decimal_to_float(_, Float64) yields a Float64"),
        },
        Some(other) => {
            unreachable!("a non-numeric percentile fraction is rejected at resolve: {other:?}")
        }
    })
}

/// `percentile_disc` over the already-sorted group values: the value at row `ceil(p·N)` (1-based),
/// i.e. the smallest `K` with `K/N ≥ p` (PG `orderedsetaggs.c`). Caller guarantees non-empty + the
/// fraction in range. Takes `&[Value]` (clones the picked value) so an array fraction can read it
/// repeatedly. spec/design/aggregates.md §13.
fn percentile_disc_at(vals: &[Value], p: f64) -> Value {
    let n = vals.len();
    let rownum = (p * n as f64).ceil() as i64;
    let idx = if rownum < 1 { 0 } else { (rownum - 1) as usize };
    let idx = idx.min(n - 1);
    vals[idx].clone()
}

/// `percentile_cont` over the already-sorted f64 group values: interpolate between the two bracketing
/// rows, in f64 with PG's exact operation order — bit-identical across cores and to PG
/// (spec/design/aggregates.md §13). Caller guarantees non-empty + the fraction in range.
fn percentile_cont_at(floats: &[f64], p: f64) -> f64 {
    let n = floats.len();
    let pos = p * ((n - 1) as f64);
    let first = pos.floor() as usize;
    let second = pos.ceil() as usize;
    if first == second {
        floats[first]
    } else {
        let lo = floats[first];
        let hi = floats[second];
        let proportion = pos - first as f64;
        lo + (proportion * (hi - lo))
    }
}

/// Apply a `WITHIN GROUP` sort direction to a comparison result (DESC reverses).
fn dir_cmp(ord: std::cmp::Ordering, desc: bool) -> std::cmp::Ordering {
    if desc { ord.reverse() } else { ord }
}

/// Sort an ordered-set aggregate's buffered values by its `WITHIN GROUP` order (aggregates.md §13).
/// With no collation, the value-canonical comparison (the same total order `ORDER BY`/`MIN`/`MAX`
/// use). With a collation, a stable decorate-sort by the precomputed collation `sort_key` bytes (a
/// collated key is always text; an unmapped code point fails `0A000` at this deterministic point,
/// like the query ORDER BY). The stable sort keeps collation-equal values in scan order, so the
/// result is deterministic and cross-core identical.
fn sort_osa_vals(vals: &mut Vec<Value>, collation: Option<&Collation>, desc: bool) -> Result<()> {
    match collation {
        None => {
            vals.sort_by(|a, b| dir_cmp(value_cmp(a, b), desc));
            Ok(())
        }
        Some(c) => {
            let mut decorated: Vec<(Vec<u8>, Value)> = Vec::with_capacity(vals.len());
            for v in vals.drain(..) {
                let key = match &v {
                    Value::Text(s) => collation::sort_key(c, s)?,
                    other => {
                        unreachable!("a collated WITHIN GROUP key buffers only text: {other:?}")
                    }
                };
                decorated.push((key, v));
            }
            decorated.sort_by(|a, b| dir_cmp(a.0.cmp(&b.0), desc));
            vals.extend(decorated.into_iter().map(|(_, v)| v));
            Ok(())
        }
    }
}

/// The percentile fraction range gate (spec/design/aggregates.md §13): `< 0`, `> 1`, or NaN is
/// `22003` (`numeric_value_out_of_range`), matching PG's "percentile value … is not between 0
/// and 1". Called per group at finalize, after the NULL-fraction check.
fn check_percentile_fraction(p: f64) -> Result<()> {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return Err(EngineError::new(
            SqlState::NumericValueOutOfRange,
            format!("percentile value {p} is not between 0 and 1"),
        ));
    }
    Ok(())
}

/// Widen a numeric value to f64 for `percentile_cont` (spec/design/aggregates.md §13): integers via
/// the IEEE cast, decimals via the correctly-rounded `decimal→f64` cast (matching PG's
/// `numeric→float8`), floats unchanged (f32 widened to its exact f64). The resolver restricts the
/// operand to a numeric family, so no other variant reaches here.
fn percentile_input_f64(v: &Value) -> Result<f64> {
    Ok(match v {
        Value::Int(i) => *i as f64,
        Value::Float32(f) => *f as f64,
        Value::Float64(f) => *f,
        Value::Decimal(d) => match decimal_to_float(d, ScalarType::Float64)? {
            Value::Float64(f) => f,
            _ => unreachable!("decimal_to_float(_, Float64) yields a Float64"),
        },
        _ => unreachable!("resolver restricts percentile_cont to a numeric operand"),
    })
}

/// Whether any select item contains an aggregate call — i.e. this is an aggregate query.
fn items_have_aggregate(items: &SelectItems) -> bool {
    match items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_aggregate(&it.expr)),
    }
}

/// Whether a window definition's PARTITION BY / ORDER BY keys contain an aggregate (`OVER (ORDER BY
/// sum(x))` — spec/design/window.md §5.1). Such an aggregate makes the query an aggregate query (a
/// whole-table aggregate if there is no GROUP BY), exactly as a top-level aggregate would, so the
/// window keys resolve against the grouped row. Used by both the inline-`over` walk in
/// `expr_has_aggregate` and the WINDOW-clause scan that computes `is_agg`.
fn window_def_has_aggregate(wd: &WindowDef) -> bool {
    wd.partition.iter().any(expr_has_aggregate)
        || wd.order.iter().any(|k| expr_has_aggregate(&k.expr))
}

/// Whether any WINDOW-clause entry's keys contain an aggregate (`WINDOW w AS (ORDER BY sum(x))`),
/// which — like a top-level aggregate — makes the query an aggregate query (spec/design/window.md
/// §5.1). The entries are still named references at this point (the OVER-name desugar runs later), so
/// the WINDOW clause is scanned directly.
fn windows_have_aggregate(windows: &[(String, WindowDef)]) -> bool {
    windows.iter().any(|(_, wd)| window_def_has_aggregate(wd))
}

/// The sub-expressions of one AST subscript spec (an index, or a slice's present bounds) — for the
/// `Expr` tree walkers.
fn subscript_spec_exprs(s: &SubscriptSpec) -> Vec<&Expr> {
    match s {
        SubscriptSpec::Index(i) => vec![i],
        SubscriptSpec::Slice(lo, hi) => lo.iter().chain(hi.iter()).collect(),
    }
}

/// Whether an expression tree contains an AGGREGATE call anywhere. A scalar-function call is
/// not itself an aggregate, but may CONTAIN one (`abs(sum(x))`), so its arguments are walked.
fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            name,
            args,
            over,
            over_name,
            within_group,
            ..
        } => {
            // An aggregate name carrying OVER (inline or a named-window reference) is a WINDOW
            // function, not a bare aggregate (S3/S5, spec/design/window.md §5.1) — so it does not
            // make the query an aggregate query. (Detection runs before the OVER-name desugar.) But an
            // aggregate INSIDE its inline window definition's keys (`rank() OVER (ORDER BY sum(x))`)
            // does — those keys resolve against the grouped row (§5.1). A hypothetical-set name with a
            // WITHIN GROUP clause (`rank(x) WITHIN GROUP (…)`) is an aggregate (aggregates.md §19).
            (over.is_none() && over_name.is_none() && is_aggregate_name(name))
                || (within_group.is_some() && is_hypothetical_set_name(name))
                || args.iter().any(expr_has_aggregate)
                || over.as_deref().is_some_and(window_def_has_aggregate)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_aggregate(inner),
        Expr::Collate { inner, .. } => expr_has_aggregate(inner),
        Expr::Unary { operand, .. } => expr_has_aggregate(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_aggregate(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_has_aggregate(ctx) || expr_has_aggregate(path),
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_has_aggregate(lhs) || list.iter().any(expr_has_aggregate)
        }
        Expr::Quantified { lhs, array, .. } => expr_has_aggregate(lhs) || expr_has_aggregate(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(lo) || expr_has_aggregate(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().any(expr_has_aggregate),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_aggregate(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_aggregate(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_aggregate)
        }
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
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Whether any select item contains a window-function call (a `FuncCall` carrying `OVER`). A
/// window query resolves its projection in `AggCtx::Window` mode (spec/design/window.md §5.1).
fn items_have_window(items: &SelectItems) -> bool {
    match items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_window(&it.expr)),
    }
}

/// Whether any ORDER BY key is (or contains) a window function, so a query whose only `OVER` call
/// sits in the ORDER BY still sets up the window machinery (grammar.md §10, window.md §5.1). An
/// ordinal/column key carries no expression.
fn order_by_has_window(keys: &[OrderKey]) -> bool {
    keys.iter()
        .any(|k| k.expr.as_ref().is_some_and(expr_has_window))
}

/// Whether an expression tree contains a window-function call anywhere (a `FuncCall` whose `over`
/// is set). An ordinary call may CONTAIN one in its arguments (`abs(row_number() OVER ())`), so the
/// arguments are walked; a window call's own PARTITION BY / ORDER BY may not contain a window
/// function (that is rejected at resolve, 42P20), so they are not walked here.
fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => over.is_some() || over_name.is_some() || args.iter().any(expr_has_window),
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_window(inner),
        Expr::Collate { inner, .. } => expr_has_window(inner),
        Expr::Unary { operand, .. } => expr_has_window(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_window(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_has_window(ctx) || expr_has_window(path),
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::In { lhs, list, .. } => expr_has_window(lhs) || list.iter().any(expr_has_window),
        Expr::Quantified { lhs, array, .. } => expr_has_window(lhs) || expr_has_window(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_window(lhs) || expr_has_window(lo) || expr_has_window(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().any(expr_has_window),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_window(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_window(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_window)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_window)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_window(c) || expr_has_window(r))
                || els.as_deref().is_some_and(expr_has_window)
        }
        // A subquery is an independent query: a window function inside it is the subquery's own.
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Desugar `OVER name` references in a select list to their WINDOW-clause definitions before
/// resolution (spec/design/window.md §5): each window call carrying `over_name` gets the named
/// definition copied into `over`; an undefined name is 42704. After this every window call carries
/// an inline `over`, so resolution (S0–S4) handles named and inline windows uniformly.
fn desugar_items(items: &mut SelectItems, windows: &[(String, WindowDef)]) -> Result<()> {
    if let SelectItems::Items(v) = items {
        for it in v.iter_mut() {
            desugar_named_windows(&mut it.expr, windows)?;
        }
    }
    Ok(())
}

/// Apply the base-window merge rules (spec/design/window.md §5, PostgreSQL
/// `transformWindowDefinitions`): a definition that names a base copies the base's `PARTITION BY`
/// and — if the base has one — its `ORDER BY`, and supplies its own frame. The extender may **not**
/// add a `PARTITION BY` (42P20, even when the base has none), may add an `ORDER BY` only when the
/// base has none (42P20 otherwise), and the base must **not** carry a frame (42P20). The three
/// checks fire in PostgreSQL's priority order: PARTITION, then ORDER, then frame. Returns the
/// merged inline definition (`base = None`).
fn extend_window(base: &WindowDef, ext: &WindowDef, base_name: &str) -> Result<WindowDef> {
    if !ext.partition.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override PARTITION BY clause of window \"{base_name}\""),
        ));
    }
    if !base.order.is_empty() && !ext.order.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override ORDER BY clause of window \"{base_name}\""),
        ));
    }
    if base.frame.is_some() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot copy window \"{base_name}\" because it has a frame clause"),
        ));
    }
    Ok(WindowDef {
        base: None,
        partition: base.partition.clone(),
        order: if base.order.is_empty() {
            ext.order.clone()
        } else {
            base.order.clone()
        },
        frame: ext.frame.clone(),
    })
}

/// Resolve a WINDOW clause into all-inline definitions (spec/design/window.md §5). Entries are
/// processed left-to-right; an entry naming a base extends an **already-resolved earlier** entry
/// (a self- or forward-reference is therefore "does not exist" — 42704), via `extend_window`. Every
/// entry is resolved — even ones no `OVER` references — matching PostgreSQL's whole-clause check.
fn resolve_window_clause(windows: &[(String, WindowDef)]) -> Result<Vec<(String, WindowDef)>> {
    let mut resolved: Vec<(String, WindowDef)> = Vec::with_capacity(windows.len());
    for (name, def) in windows {
        let r = if let Some(base_name) = &def.base {
            let base = lookup_window(&resolved, base_name)?;
            extend_window(&base, def, base_name)?
        } else {
            def.clone()
        };
        resolved.push((name.clone(), r));
    }
    Ok(resolved)
}

/// Find a (resolved, `base = None`) window definition by name in `windows`, case-insensitively, or
/// raise 42704 `window "<name>" does not exist`. Returns an owned clone to avoid borrow conflicts.
fn lookup_window(windows: &[(String, WindowDef)], name: &str) -> Result<WindowDef> {
    windows
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, d)| d.clone())
        .ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedObject,
                format!("window \"{name}\" does not exist"),
            )
        })
}

fn desugar_named_windows(e: &mut Expr, windows: &[(String, WindowDef)]) -> Result<()> {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => {
            if let Some(name) = over_name.take() {
                // `OVER name` — a pure reference: copy the named definition whole, frame included
                // (no merge rules; copying a framed window is only forbidden for the parenthesized
                // extend form below — window.md §5).
                let def = lookup_window(windows, &name)?;
                *over = Some(Box::new(def));
            } else if over.as_ref().is_some_and(|d| d.base.is_some()) {
                // `OVER (base …)` — an extend: merge the inline definition onto the named base.
                let d = over.as_deref_mut().expect("base implies over is Some");
                let base_name = d.base.take().expect("base.is_some() checked");
                let base = lookup_window(windows, &base_name)?;
                *d = extend_window(&base, d, &base_name)?;
            }
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        Expr::Cast { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Collate { inner, .. } => {
            desugar_named_windows(inner, windows)?;
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => {
            desugar_named_windows(operand, windows)?;
        }
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            desugar_named_windows(ctx, windows)?;
            desugar_named_windows(path, windows)?;
        }
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::In { lhs, list, .. } => {
            desugar_named_windows(lhs, windows)?;
            for x in list.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::Quantified { lhs, array, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(array, windows)?;
        }
        Expr::Between { lhs, lo, hi, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(lo, windows)?;
            desugar_named_windows(hi, windows)?;
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::Row(items) | Expr::Array(items) => {
            for x in items.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            desugar_named_windows(base, windows)?;
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(o) = operand.as_deref_mut() {
                desugar_named_windows(o, windows)?;
            }
            for (c, r) in whens.iter_mut() {
                desugar_named_windows(c, windows)?;
                desugar_named_windows(r, windows)?;
            }
            if let Some(x) = els.as_deref_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        // Leaves, subscripts, and subqueries (independent) carry no top-level window ref to rewrite.
        _ => {}
    }
    Ok(())
}

/// The structural CHECK-expression rejections (spec/design/constraints.md §4.1), applied in
/// a single depth-first pre-order walk before resolution: a subquery is 0A000, an aggregate
/// call 42803, a bind parameter 42P02 — PG's codes and messages (oracle-probed; PG
/// interleaves these with resolution in parse order, a documented micro-order divergence).
fn reject_check_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in check constraint",
        )),
        Expr::Param(n) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in check constraints",
                ));
            }
            args.iter().try_for_each(reject_check_structure)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_check_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_check_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_check_structure(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            reject_check_structure(ctx)?;
            reject_check_structure(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_check_structure(lhs)?;
            list.iter().try_for_each(reject_check_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(array)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().try_for_each(reject_check_structure),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_check_structure(base),
        // `t.*` cannot syntactically reach a CHECK expression (it is a select-item-only shape —
        // `CHECK (t.*)` is a 42601 in the parser); accept it structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_check_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_check_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(lo)?;
            reject_check_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(op) = operand {
                reject_check_structure(op)?;
            }
            for (c, r) in whens {
                reject_check_structure(c)?;
                reject_check_structure(r)?;
            }
            match els {
                Some(e) => reject_check_structure(e),
                None => Ok(()),
            }
        }
    }
}

/// The structural rejections for a `DEFAULT` expression (constraints.md §2), a single
/// depth-first pre-walk run before name/type resolution (the same micro-order divergence from
/// PG that `reject_check_structure` carries). A default extends the CHECK rejections with one
/// more: it may **not reference a column** (it is computed before the row exists). Codes match
/// PostgreSQL (oracle-probed): a column reference / subquery is `0A000`, an aggregate `42803`,
/// a bind parameter `42P02`.
fn reject_default_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::Column(_) | Expr::QualifiedColumn { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use column reference in DEFAULT expression",
        )),
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in DEFAULT expression",
        )),
        Expr::Param(n) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in DEFAULT expressions",
                ));
            }
            args.iter().try_for_each(reject_default_structure)
        }
        Expr::Literal(_) | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_default_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_default_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_default_structure(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            reject_default_structure(ctx)?;
            reject_default_structure(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_default_structure(lhs)?;
            list.iter().try_for_each(reject_default_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(array)
        }
        Expr::Row(items) | Expr::Array(items) => {
            items.iter().try_for_each(reject_default_structure)
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_default_structure(base),
        // `t.*` cannot syntactically reach a DEFAULT expression (select-item-only); accept
        // structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_default_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_default_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(lo)?;
            reject_default_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(op) = operand {
                reject_default_structure(op)?;
            }
            for (c, r) in whens {
                reject_default_structure(c)?;
                reject_default_structure(r)?;
            }
            match els {
                Some(e) => reject_default_structure(e),
                None => Ok(()),
            }
        }
    }
}

/// The distinct columns a CHECK expression references, as indices into `columns` — the input
/// to PG's auto-naming rule (constraints.md §4.3: exactly one distinct column →
/// `<table>_<col>_check`). Resolution already validated every reference, so an unknown name
/// is simply skipped; a qualified reference counts its column like a bare one (oracle-probed).
fn check_referenced_columns(e: &Expr, columns: &[Column]) -> Vec<usize> {
    fn walk(e: &Expr, columns: &[Column], out: &mut Vec<usize>) {
        let mut note = |name: &str| {
            if let Some(i) = columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
            {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        };
        match e {
            Expr::Column(name) | Expr::QualifiedColumn { name, .. } => note(name),
            Expr::Literal(_) | Expr::TypedLiteral { .. } | Expr::Param(_) => {}
            Expr::Cast { inner, .. }
            | Expr::Collate { inner, .. }
            | Expr::Extract { source: inner, .. } => walk(inner, columns, out),
            Expr::Unary { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::IsJson { operand, .. }
            | Expr::JsonCtor { operand, .. } => walk(operand, columns, out),
            Expr::JsonExists { ctx, path, .. }
            | Expr::JsonValue { ctx, path, .. }
            | Expr::JsonQuery { ctx, path, .. } => {
                walk(ctx, columns, out);
                walk(path, columns, out);
            }
            Expr::Binary { lhs, rhs, .. }
            | Expr::IsDistinctFrom { lhs, rhs, .. }
            | Expr::Like { lhs, rhs, .. }
            | Expr::Regex { lhs, rhs, .. } => {
                walk(lhs, columns, out);
                walk(rhs, columns, out);
            }
            Expr::In { lhs, list, .. } => {
                walk(lhs, columns, out);
                for x in list {
                    walk(x, columns, out);
                }
            }
            Expr::Quantified { lhs, array, .. } => {
                walk(lhs, columns, out);
                walk(array, columns, out);
            }
            Expr::Between { lhs, lo, hi, .. } => {
                walk(lhs, columns, out);
                walk(lo, columns, out);
                walk(hi, columns, out);
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                if let Some(op) = operand {
                    walk(op, columns, out);
                }
                for (c, r) in whens {
                    walk(c, columns, out);
                    walk(r, columns, out);
                }
                if let Some(e) = els {
                    walk(e, columns, out);
                }
            }
            Expr::FuncCall { args, .. } => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::Row(items) | Expr::Array(items) => {
                for it in items {
                    walk(it, columns, out);
                }
            }
            Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => walk(base, columns, out),
            // `t.*` cannot appear in a CHECK expression (select-item-only); no columns to note.
            Expr::QualifiedStar { .. } => {}
            Expr::Subscript { base, subscripts } => {
                walk(base, columns, out);
                for e in subscripts.iter().flat_map(subscript_spec_exprs) {
                    walk(e, columns, out);
                }
            }
            // Unreachable in a validated check (rejected by `reject_check_structure`).
            Expr::ScalarSubquery(_)
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. } => {}
        }
    }
    let mut out = Vec::new();
    walk(e, columns, &mut out);
    out
}

/// The environment threaded into the per-row evaluator (spec/design/grammar.md §26): the
/// engine (to run a correlated subquery's plan), the bound parameters, and the stack of
/// enclosing rows (innermost LAST) a correlated reference reads. `outer` is empty at the top
/// level; a correlated subquery pushes the current row before running its inner plan, so an
/// `OuterColumn { level, index }` reads `outer[outer.len() - level][index]`.
pub(crate) struct EvalEnv<'a> {
    exec: &'a Engine,
    params: &'a [Value],
    outer: &'a [&'a [Value]],
    /// The per-statement entropy+clock state (spec/design/entropy.md §5): the uuidv7 monotonic
    /// counter + the once-resolved statement clock, behind a `Cell` (interior mutability — `EvalEnv`
    /// is `&`-shared; the draw order is fixed by eval order). The injected random/clock functions
    /// live on `exec.seam` (handle-scoped); only the volatile uuid generators touch any of this.
    rng: &'a std::cell::Cell<crate::seam::StmtRng>,
    /// The statement's CTE execution context (spec/design/cte.md §5), so a FROM reference at any
    /// nesting depth delivers a CTE's rows. `CteCtx::empty()` for every non-`WITH` statement.
    ctes: CteCtx<'a>,
}

/// Whether `plan` is the single-table, no-blocking-operator **streaming scan** shape
/// (spec/design/cost.md §3, streaming.md §4) — a single relation, no join / aggregate / window, an
/// output order the primary-key scan already yields (`pk_ordered`, or no `ORDER BY` with a `LIMIT`
/// short-circuit), no index/GIN/GiST bound (those read the full admitted set eagerly), and a real
/// table store (not an SRF / CTE / derived source). Both [`exec_select_plan`](Engine::exec_select_plan)
/// (which routes to the eager `exec_streaming_scan`) and [`try_scan_query`](Engine::try_scan_query)
/// (the lazy `query()` lane) gate on this ONE predicate, so the two never drift.
fn streaming_scan_eligible(plan: &SelectPlan) -> bool {
    plan.rels.len() == 1
        && plan.joins.is_empty()
        && !plan.is_agg
        && !plan.has_window
        && (plan.pk_ordered || (!plan.distinct && plan.order.is_empty() && plan.limit.is_some()))
        && !matches!(
            plan.rel_bounds[0],
            Some(ScanBound::Index(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::IndexSet(_))
        )
        && plan.rels[0].srf.is_none()
        && plan.rels[0].cte.is_none()
        && plan.rels[0].derived.is_none()
}

/// Whether `plan` is a shape [`project_columnar`](Engine::project_columnar) specializes: a bare-column
/// projection over a single base table with no join / aggregate / window / DISTINCT / ORDER BY / LIMIT /
/// OFFSET and no index/GIN/GiST bound — a plain `SELECT c0, c3, … FROM t [WHERE …]` whose output is the
/// (optionally filtered) scan-order rows narrowed to a column subset. A residual filter is allowed (A3):
/// `project_columnar` applies it over the lanes into a selection vector. Pure plan inspection (charges
/// nothing), so a bail is free and the general materialize path runs with identical results + cost; the
/// store / paging / spillable / column-range gates live in `project_columnar`, which declines to that
/// path. LIMIT/OFFSET is excluded deliberately: a LIMIT with no ORDER BY streams with an early exit
/// ([`streaming_scan_eligible`]), which the whole-table gather must not steal.
fn vectorized_project_eligible(plan: &SelectPlan) -> bool {
    if plan.is_agg || plan.has_window || plan.distinct {
        return false;
    }
    if plan.rels.len() != 1 || !plan.joins.is_empty() {
        return false;
    }
    let rel = &plan.rels[0];
    if rel.srf.is_some() || rel.cte.is_some() || rel.derived.is_some() || rel.lateral {
        return false;
    }
    // No ORDER BY / LIMIT / OFFSET (those route to a streaming / sort / index path). A residual filter is
    // fine — project_columnar vectorizes it (A3).
    if !plan.order.is_empty() || plan.limit.is_some() || plan.offset.is_some() {
        return false;
    }
    // Full scan or a primary-key bound only — an index / GIN / GiST bound changes the scan mechanics.
    if matches!(
        plan.rel_bounds[0],
        Some(ScanBound::Index(_))
            | Some(ScanBound::Gin(_))
            | Some(ScanBound::Gist(_))
            | Some(ScanBound::PkSet(_))
            | Some(ScanBound::IndexSet(_))
    ) {
        return false;
    }
    // Every projection must be a bare column reference: a bare `RExpr::Column` evaluates to `row[index]`
    // with zero operator_eval, so gathering it from a dense lane is cost-identical. An expression
    // projection (`c0 + 1`, a function call) charges operator_eval and needs a row — it keeps the row path.
    if plan.projections.is_empty() {
        return false;
    }
    plan.projections
        .iter()
        .all(|p| matches!(p, RExpr::Column(_)))
}

/// Evaluate `filter` over the gathered per-column lanes and return the surviving row indices (the
/// selection vector) — filter vectorization (packed-leaf.md §11 Track A3). It reuses the scalar
/// [`RExpr::eval`] verbatim over a SINGLE reusable scratch row (the masked columns filled from the lanes
/// at that row index, untouched columns left `Null`), so the predicate's `operator_eval` charges and its
/// 3VL survivor test (keep iff `TRUE`) are byte-identical to the scalar `WHERE` loop — and the result is
/// identical too, because the row path also feeds the filter a MASKED row (untouched columns `Null` via
/// resolve_columns / row_at_masked) and the filter references only masked columns (`collect_touched`
/// includes the filter), so a scratch row filled from the lanes is the same input. The one reusable
/// scratch row is the allocation win: no full-width row per scanned row, only the `i32` survivor indices.
/// The caller has verified no touched column spills, so every masked lane is a non-empty `Vec<Value>` of
/// length `row_count` (an untouched column's lane stays empty but is never read).
fn filter_columnar(
    filter: &RExpr,
    cols: &[Vec<Value>],
    mask: &[bool],
    row_count: usize,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<i32>> {
    let mut sel = Vec::new();
    let mut scratch: Vec<Value> = vec![Value::Null; mask.len()];
    for i in 0..row_count {
        for (c, &m) in mask.iter().enumerate() {
            if m {
                scratch[c] = cols[c][i].clone();
            }
        }
        if filter.eval(&scratch, env, meter)?.is_true() {
            sel.push(i as i32);
        }
    }
    Ok(sel)
}

/// Whether one aggregate is a specialized numeric kernel the vectorized aggregate path folds: a plain
/// (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) `COUNT(*)` / `COUNT(col)` /
/// `SUM(i16|i32)` / `SUM`|`AVG(f32|f64)` / `MIN(col)` / `MAX(col)` whose operand (where it has one) is
/// a bare column reference. `SUM(i64|decimal)` and `AVG(decimal)` are deferred (their fold charges
/// running-sum-dependent decimal_work); `MIN`/`MAX` fold ANY type through `value_cmp`. Reusing the
/// shared [`Acc::fold`] keeps the fold byte-identical to the scalar path (the scalar grouped path folds
/// through the same `Acc::fold`), so only the group/scan machinery differs.
fn vectorized_spec_eligible(spec: &AggSpec) -> bool {
    if spec.distinct || spec.filter.is_some() || spec.osa.is_some() || spec.hypo.is_some() {
        return false;
    }
    match spec.plan {
        AggPlan::CountStar => spec.operand.is_none(),
        AggPlan::Count
        | AggPlan::SumInt
        | AggPlan::SumFloat(_)
        | AggPlan::AvgFloat(_)
        | AggPlan::Min
        | AggPlan::Max => matches!(spec.operand, Some(RExpr::Column(_))),
        _ => false,
    }
}

/// The bare-column ordinal an eligible aggregate reads (its operand `RExpr::Column(idx)`), or `None`
/// for `COUNT(*)` (which folds no value). Eligibility ([`vectorized_spec_eligible`]) guarantees the
/// operand is either absent or a bare column, so this is total over an eligible spec.
fn operand_col(spec: &AggSpec) -> Option<usize> {
    match &spec.operand {
        Some(RExpr::Column(i)) => Some(*i),
        _ => None,
    }
}

/// The survivor value source for the vectorized fold — the ONE seam that differs between the row path
/// (a `Vec<Row>` of full rows) and the columnar path (dense per-column lanes + an optional A3 selection
/// vector). `at(j, col)` reads survivor `j`'s value in column `col`, so the fold kernels below are
/// written once and run either way. Cost is unaffected: both feed the same values in scan order.
pub(crate) enum LaneSrc<'a> {
    /// The row path: survivors are full rows; `at(j, col)` is `rows[j][col]`.
    Rows(&'a [Row]),
    /// The columnar path: `cols[col]` is a dense lane; `sel` (A3) maps survivor `j` to lane index
    /// `sel[j]` (or `j` itself when there is no filter).
    Cols {
        cols: &'a [Vec<Value>],
        sel: Option<&'a [i32]>,
    },
}

impl LaneSrc<'_> {
    #[inline]
    fn at(&self, j: usize, col: usize) -> &Value {
        match self {
            LaneSrc::Rows(rows) => &rows[j][col],
            LaneSrc::Cols { cols, sel } => {
                let i = match sel {
                    Some(s) => s[j] as usize,
                    None => j,
                };
                &cols[col][i]
            }
        }
    }
}

/// Fold one WHOLE-TABLE grand-total group over `nsurv` survivors from `src`, returning the finalized
/// aggregate results `[agg_0, …]` (the synthetic row for a `()` group — no key columns). It builds one
/// [`Acc`] per spec and folds each survivor's operand value through the shared [`Acc::fold`] (identical
/// acc state, hence [`Acc::finalize`], to the scalar path), charging `aggregate_accumulate` once per
/// (survivor × spec) in bulk — the identical total to the scalar loop (which charges per row × spec),
/// and cost-safe because the caller gates to the unmetered lane (no per-row guard to preserve).
fn fold_agg_whole(
    specs: &[AggSpec],
    src: &LaneSrc,
    nsurv: usize,
    meter: &mut Meter,
) -> Result<Vec<Value>> {
    let mut accs: Vec<Acc> = specs.iter().map(Acc::from_spec).collect();
    for (si, spec) in specs.iter().enumerate() {
        meter.charge(COSTS.aggregate_accumulate * nsurv as i64);
        let oc = operand_col(spec);
        for j in 0..nsurv {
            let v = match oc {
                Some(c) => src.at(j, c).clone(),
                None => Value::Null, // COUNT(*) folds no value
            };
            accs[si].fold(v, meter)?;
        }
    }
    accs.into_iter().map(Acc::finalize).collect()
}

/// Bucket `nsurv` survivors from `src` by their single INTEGER group-key column and fold each aggregate
/// per group, returning the finalized synthetic rows `[key, agg_0, …]` in scan-order-of-first-
/// appearance. The bucket is a `HashMap<i64, usize>` over the raw key (a bijection of the scalar path's
/// value-canonical group key for a fixed-width integer column) plus one sentinel group for NULL keys
/// (the value-canonical key groups all NULLs together). The fold reuses [`Acc::fold`] (byte-identical
/// acc state); `aggregate_accumulate` is charged once per (survivor × spec) in bulk — the identical
/// total to the scalar loop. The bucketing itself is unmetered (cost.md §3), so the `i64` map is a free
/// internal choice. The caller has verified the key lane (and each operand lane) is populated.
fn group_by_int_key(
    specs: &[AggSpec],
    key_col: usize,
    src: &LaneSrc,
    nsurv: usize,
    meter: &mut Meter,
) -> Result<Vec<Vec<Value>>> {
    let mut groups: Vec<(Value, Vec<Acc>)> = Vec::new();
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut null_gi: Option<usize> = None;

    meter.charge(COSTS.aggregate_accumulate * nsurv as i64 * specs.len() as i64);
    for j in 0..nsurv {
        let kv = src.at(j, key_col);
        let gi = match kv {
            Value::Int(k) => match index.get(k) {
                Some(&g) => g,
                None => {
                    let g = groups.len();
                    index.insert(*k, g);
                    groups.push((kv.clone(), specs.iter().map(Acc::from_spec).collect()));
                    g
                }
            },
            // A NULL integer key (the only other case for an integer column) buckets into one sentinel
            // group, exactly as the scalar path groups all NULLs together.
            _ => match null_gi {
                Some(g) => g,
                None => {
                    let g = groups.len();
                    null_gi = Some(g);
                    groups.push((Value::Null, specs.iter().map(Acc::from_spec).collect()));
                    g
                }
            },
        };
        let accs = &mut groups[gi].1;
        for (si, spec) in specs.iter().enumerate() {
            let v = match operand_col(spec) {
                Some(c) => src.at(j, c).clone(),
                None => Value::Null,
            };
            accs[si].fold(v, meter)?;
        }
    }

    groups
        .into_iter()
        .map(|(key, accs)| {
            let mut srow: Vec<Value> = Vec::with_capacity(1 + accs.len());
            srow.push(key);
            for a in accs {
                srow.push(a.finalize()?);
            }
            Ok(srow)
        })
        .collect()
}

/// A prepared statement's memoized scan plan (spec/design/api.md §2.4): the resolved [`SelectPlan`]
/// (shared `Rc`, so a cache hit rebuilds the cursor around the SAME plan allocation and re-plans
/// nothing) plus the finalized `$N` param types, stamped with the [`Database`](crate::Database)
/// (shared core) and committed catalog generation they were resolved against. A statement is a
/// standalone value shared across sessions, so a hit requires the same core — `cat_gen` is only
/// monotonic within one core; two databases can share a generation number with different schemas —
/// AND the same generation (any DDL bumps it and the next execute re-plans), and re-checks that no
/// plan relation is shadowed by the executing session's temp domain ([`Engine::plan_touches_temp`]).
/// Filled only for a reusable plan read from committed state ([`Engine::try_scan_query`]). The plan
/// is `!Send` (it holds a regex `Cell`), so a `PreparedStatement` carrying one is `!Send` too — a
/// non-regression, the whole query/cursor path is already thread-affine.
pub(crate) struct CachedPlan {
    // Fields are private to the executor: api.rs / shared.rs only name the type (to hold the
    // `RefCell<Option<CachedPlan>>` cache and thread it), never touch the fields — which keeps the
    // more-private `SelectPlan` out of a pub(crate) field.
    //
    // `core` is a `Weak` so a statement outliving its `Database` does not keep the core's storage
    // alive — and the weak count keeps the allocation address from being reused, so the `ptr_eq`
    // identity check cannot alias a later database (no ABA).
    core: std::sync::Weak<crate::shared::Shared>,
    cat_gen: u64,
    plan: std::rc::Rc<SelectPlan>,
    param_types: Vec<ScalarType>,
}

/// The lazy pull pipeline behind a streaming [`Rows`](crate::Rows) cursor (spec/design/streaming.md
/// §3/§4, S3): [`exec_streaming_scan`](Engine::exec_streaming_scan)'s per-row loop turned inside out
/// so the CALLER pulls each row. It owns a frozen snapshot [`Engine`] (eval's `exec`, so the cursor
/// is self-contained and outlives the handle — streaming.md §5), a pull B-tree
/// [`StoreScan`](crate::storage::StoreScan) over that snapshot (the scan pin), the resolved + folded
/// plan, bound params, a per-statement entropy cell, and its own cost [`Meter`]. Each
/// [`next_row`](crate::cursor::RowStream::next_row) runs scan → resolve touched columns → `WHERE` →
/// project for ONE output row, accruing the identical cost units at the identical sites as the eager
/// path — so a fully-drained streaming query observes the same rows + total cost (streaming.md §6),
/// while a caller that stops early reads (and charges) less.
pub(crate) struct StreamingScan {
    engine: Engine,
    /// The resolved plan, shared (`Rc`) so a prepared statement's plan cache and this cursor hold the
    /// same allocation — a cache hit rebinds params + rebuilds the cursor but re-plans nothing
    /// (spec/design/api.md §2.4). Read-only during iteration (the fold ran before wrapping).
    plan: std::rc::Rc<SelectPlan>,
    params: Vec<Value>,
    rng: std::cell::Cell<crate::seam::StmtRng>,
    scan: crate::storage::StoreScan,
    meter: Meter,
    offset: i64,
    limit: Option<i64>,
    distinct: bool,
    seen: std::collections::HashSet<Vec<Value>>,
    /// Survivors past the filter+dedup so far (the `OFFSET` runs against this), like
    /// `exec_streaming_scan`'s `passed`.
    passed: i64,
    /// Output rows produced so far (the `LIMIT` short-circuit runs against this).
    produced: i64,
    /// Set once the scan is exhausted, the `LIMIT` window is filled, or the bound is empty —
    /// after which `next_row` short-circuits without faulting another leaf.
    done: bool,
}

impl crate::cursor::RowStream for StreamingScan {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        if self.done {
            return Ok(None);
        }
        // The LIMIT short-circuit: once the window is full, stop WITHOUT pulling another row — so no
        // further leaf is faulted (the streaming early-exit win; cost.md §3 "LIMIT short-circuit").
        if let Some(l) = self.limit
            && self.produced >= l
        {
            self.done = true;
            return Ok(None);
        }
        let env = EvalEnv {
            exec: &self.engine,
            params: &self.params,
            outer: &[],
            rng: &self.rng,
            ctes: CteCtx::empty(),
        };
        let mask = &self.plan.rel_masks[0];
        loop {
            let (_key, mut row) = match self.scan.next()? {
                Some(p) => p,
                None => {
                    self.done = true;
                    return Ok(None);
                }
            };
            self.meter.guard()?; // enforce the cost ceiling / cancellation per scanned row
            self.meter.charge(COSTS.storage_row_read);
            // Materialize the touched columns left unfetched by the lazy load (large-values.md §14);
            // the chain reads were already metered in the up-front block (cost.md §3).
            if TableStore::needs_resolution(&row, mask) {
                self.scan.resolve_columns(&mut row, mask)?;
            }
            let keep = match &self.plan.filter {
                Some(f) => f.eval(&row, &env, &mut self.meter)?.is_true(),
                None => true,
            };
            if !keep {
                continue;
            }
            if self.distinct {
                // DISTINCT (cost.md §3): project EVERY scanned filtered row (the dedup key, charged
                // even for a duplicate — the §3 asymmetry), drop a value already seen, then OFFSET/LIMIT
                // window the survivors — exactly `exec_streaming_scan`.
                let mut projected = Vec::with_capacity(self.plan.projections.len());
                for p in &self.plan.projections {
                    projected.push(p.eval(&row, &env, &mut self.meter)?);
                }
                if !self.seen.insert(projected.clone()) {
                    continue;
                }
                self.passed += 1;
                if self.passed <= self.offset {
                    continue;
                }
                self.meter.charge(COSTS.row_produced);
                self.produced += 1;
                return Ok(Some(projected));
            }
            self.passed += 1;
            if self.passed <= self.offset {
                continue;
            }
            self.meter.charge(COSTS.row_produced);
            let mut projected = Vec::with_capacity(self.plan.projections.len());
            for p in &self.plan.projections {
                projected.push(p.eval(&row, &env, &mut self.meter)?);
            }
            self.produced += 1;
            return Ok(Some(projected));
        }
    }

    fn cost(&self) -> i64 {
        self.meter.accrued
    }

    fn close(&mut self) {
        // The pinned snapshot is owned by `self.engine` / `self.scan` and released on `Drop`; mark
        // done so any further `next_row` is a no-op (streaming.md §5, idempotent).
        self.done = true;
    }
}

/// The lazy **buffered** pull pipeline behind a `query()` [`Rows`](crate::Rows) cursor for a plan with
/// a blocking operator (spec/design/streaming.md §4, S4) — the generalization of `SortedRows::next()`
/// to every blocking shape. It owns a frozen snapshot [`Engine`] (eval's `exec`, so the cursor is
/// self-contained and outlives the handle — streaming.md §5), the resolved + folded plan, bound
/// params, a per-statement entropy cell, its own cost [`Meter`], and the lazy emission `state`. On its
/// FIRST [`next_row`](crate::cursor::RowStream::next_row) it runs the blocking part
/// ([`exec_select_emit`](Engine::exec_select_emit)) to completion into an [`Emitter`] — buffering the
/// input (correctly: a sort/group/dedup/join must see it all) and charging the scan/sort/group/dedup
/// cost — then yields its buffer **one row at a time**: a `Project` row is projected (and charges
/// `row_produced` + projection) on emission, a `Sorted` row is pulled from the [`SortedRows`] iterator
/// and projected (the streaming-sort output, streaming.md §4/§7), an `Identity`/`Final` row is handed
/// out (already projected). So peak *output* memory is one row, a caller's early exit skips the
/// projection of the rows it never pulls, and a fully-drained query observes the same rows + total cost
/// as the eager path (streaming.md §6).
pub(crate) struct BufferedScan {
    engine: Engine,
    /// The resolved plan, shared (`Rc`) with a prepared statement's plan cache (see [`StreamingScan`]).
    plan: std::rc::Rc<SelectPlan>,
    params: Vec<Value>,
    rng: std::cell::Cell<crate::seam::StmtRng>,
    meter: Meter,
    state: BufState,
}

/// The lazy emission state of a [`BufferedScan`] (spec/design/streaming.md §4).
pub(crate) enum BufState {
    /// The blocking part has not run yet — the first `next_row` runs it (streaming.md §4).
    Pending,
    /// The general blocking buffer, windowed to `[idx, end)`. Each emission charges `row_produced`;
    /// `project` rows additionally evaluate the projection list (`Identity` rows are pre-projected).
    Buffer {
        rows: Vec<Vec<Value>>,
        idx: usize,
        end: usize,
        project: bool,
    },
    /// A fully-formed result from a special input-streaming path (already projected AND charged) —
    /// emission just hands the rows out.
    Final {
        iter: std::vec::IntoIter<Vec<Value>>,
    },
    /// The streaming sort's lazy output: the [`SortedRows`] pull iterator (positioned past the
    /// `OFFSET`) and `remaining` windowed rows still to emit. Each `next_row` pulls the next sorted
    /// row, charges `row_produced`, and projects it — so the output `Vec` is never built and an early
    /// exit skips the rows it never pulls (streaming.md §4/§7).
    Sorted {
        sorted: crate::spill::SortedRows,
        remaining: usize,
    },
    /// The columnar projection fast path's lazy state (packed-leaf.md §11 Track A2/A3): the pre-gathered
    /// dense lanes + the projection's column indices, windowed to `[idx, end)`, with the optional A3
    /// selection vector. Each emission gathers output row `j` from the lanes at lane position `sel[j]`
    /// (or `j`) and charges `row_produced` — an early exit skips the rows it never pulls.
    Columnar {
        cols: Vec<Vec<Value>>,
        proj_cols: Vec<usize>,
        sel: Option<Vec<i32>>,
        idx: usize,
        end: usize,
    },
    /// The buffer is exhausted (or the cursor was closed) — every further `next_row` is `None`.
    Done,
}

impl crate::cursor::RowStream for BufferedScan {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        // Run the blocking part on the FIRST pull (streaming.md §4 — `Buffered` runs the blocking part
        // then yields its buffer lazily). A mid-blocking cost abort / cancellation / trap surfaces HERE
        // (during iteration), not at `query()` time (streaming.md §6). Disjoint-field borrows: the
        // emit reads `self.engine`/`self.plan`/`self.params`/`self.rng` and writes `self.meter`, all
        // distinct from `self.state` it then assigns.
        if matches!(self.state, BufState::Pending) {
            let emitter = self.engine.exec_select_emit(
                self.plan.as_ref(),
                &[],
                &self.params,
                CteCtx::empty(),
                &self.rng,
                &mut self.meter,
            )?;
            self.state = match emitter {
                Emitter::Buffer {
                    rows,
                    start,
                    end,
                    mode,
                } => BufState::Buffer {
                    rows,
                    idx: start,
                    end,
                    project: matches!(mode, EmitMode::Project),
                },
                Emitter::Final { rows } => BufState::Final {
                    iter: rows.into_iter(),
                },
                Emitter::Sorted { sorted, remaining } => BufState::Sorted { sorted, remaining },
                Emitter::Columnar {
                    cols,
                    proj_cols,
                    sel,
                    start,
                    end,
                } => BufState::Columnar {
                    cols,
                    proj_cols,
                    sel,
                    idx: start,
                    end,
                },
            };
        }
        match &mut self.state {
            BufState::Done => Ok(None),
            BufState::Pending => unreachable!("the blocking part ran above"),
            // Already projected + charged — hand the next row out (no further cost).
            BufState::Final { iter } => Ok(iter.next()),
            // The streaming sort's lazy output: pull the next windowed row, charge `row_produced`,
            // and project it (streaming.md §4/§7). Disjoint-field borrows: `sorted`/`remaining` come
            // from `self.state`, distinct from `self.meter`/`self.engine`/`self.plan`/`self.rng`/
            // `self.params` the projection reads.
            BufState::Sorted { sorted, remaining } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                let row = sorted
                    .next()?
                    .expect("the sorter yields exactly the windowed rows");
                *remaining -= 1;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                let env = EvalEnv {
                    exec: &self.engine,
                    params: &self.params,
                    outer: &[],
                    rng: &self.rng,
                    ctes: CteCtx::empty(),
                };
                let mut out = Vec::with_capacity(self.plan.projections.len());
                for p in &self.plan.projections {
                    out.push(p.eval(&row, &env, &mut self.meter)?);
                }
                Ok(Some(out))
            }
            BufState::Buffer {
                rows,
                idx,
                end,
                project,
            } => {
                if *idx >= *end {
                    return Ok(None);
                }
                let i = *idx;
                *idx += 1;
                let project = *project;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                if project {
                    let env = EvalEnv {
                        exec: &self.engine,
                        params: &self.params,
                        outer: &[],
                        rng: &self.rng,
                        ctes: CteCtx::empty(),
                    };
                    let mut out = Vec::with_capacity(self.plan.projections.len());
                    for p in &self.plan.projections {
                        out.push(p.eval(&rows[i], &env, &mut self.meter)?);
                    }
                    Ok(Some(out))
                } else {
                    Ok(Some(std::mem::take(&mut rows[i])))
                }
            }
            // Columnar projection (packed-leaf.md §11 Track A2/A3): gather this row from the dense lanes —
            // a bare-column projection with no full-width row — charging only row_produced (a bare column
            // ref is a zero-cost slot read). A non-None `sel` (the A3 filter's survivors) maps output row
            // j to lane position sel[j].
            BufState::Columnar {
                cols,
                proj_cols,
                sel,
                idx,
                end,
            } => {
                if *idx >= *end {
                    return Ok(None);
                }
                let j = *idx;
                *idx += 1;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                let l = match sel {
                    Some(s) => s[j] as usize,
                    None => j,
                };
                let mut out = Vec::with_capacity(proj_cols.len());
                for &c in proj_cols.iter() {
                    out.push(cols[c][l].clone());
                }
                Ok(Some(out))
            }
        }
    }

    fn cost(&self) -> i64 {
        self.meter.accrued
    }

    fn close(&mut self) {
        // The pinned snapshot is owned by `self.engine` and released on `Drop`; mark done so any
        // further `next_row` is a no-op (streaming.md §5, idempotent).
        self.state = BufState::Done;
    }
}

/// A top-level set operation / pure-query `WITH` deferred to a lazy cursor (spec/design/streaming.md
/// §4/§7). Its output is already projected + charged, so there is no per-row projection to defer — the
/// cursor's only job is to run the whole query on the FIRST pull and yield the result one row at a
/// time. Owned by a [`DeferredResult`]; run via the eager `run_set_op` / `run_with` verbatim so the
/// rows + cost match `execute()` exactly (§6).
pub(crate) enum DeferredQuery {
    SetOp(SetOp),
    With(WithQuery),
}

/// The lazy **deferred** pull pipeline behind a `query()` [`Rows`](crate::Rows) cursor for a top-level
/// set operation / pure-query `WITH` (spec/design/streaming.md §7). It owns a frozen snapshot
/// [`Engine`] (§5), the owned query AST, and the bound params; on its FIRST
/// [`next_row`](crate::cursor::RowStream::next_row) it runs the whole `run_set_op` / `run_with` to
/// completion (so a cost abort / cancellation / trap surfaces *during iteration*, not at `query()` —
/// §6), records the accrued cost, and yields the materialized result **one row at a time**. The input
/// is still buffered (a set op dedups / a `WITH` materializes — it must), so the win here is only
/// lazy-yield: the work is deferred to the first pull and the result rows are handed out incrementally
/// rather than wrapped in an eager `Outcome`. Under full drain the rows + total cost are byte-identical
/// to the eager path (it drives the SAME `run_*`, §6).
pub(crate) struct DeferredResult {
    engine: Engine,
    /// The query to run, taken on the first pull (`None` afterwards).
    query: Option<DeferredQuery>,
    params: Vec<Value>,
    state: DeferredState,
    /// The accrued cost — 0 until the first pull runs the query, then `SelectResult::cost` (final).
    cost: i64,
}

/// The lazy emission state of a [`DeferredResult`] (spec/design/streaming.md §7).
pub(crate) enum DeferredState {
    /// The query has not run yet — the first `next_row` runs it (streaming.md §7).
    Pending,
    /// The materialized result, walked one row at a time.
    Yielding(std::vec::IntoIter<Vec<Value>>),
    /// Exhausted (or the cursor was closed) — every further `next_row` is `None`.
    Done,
}

impl crate::cursor::RowStream for DeferredResult {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        // Run the whole set op / WITH on the FIRST pull (streaming.md §7), reusing the eager
        // `run_set_op` / `run_with` verbatim so the rows + cost match `execute()` exactly. A mid-run
        // cost abort / cancellation / arithmetic trap surfaces HERE (during iteration), not at
        // `query()` (streaming.md §6). `query.take()` releases its borrow before the `&self.engine`
        // run, so the later `self.cost`/`self.state` writes do not alias.
        if let Some(query) = self.query.take() {
            let r = match query {
                DeferredQuery::SetOp(so) => self.engine.run_set_op(so, &self.params)?,
                DeferredQuery::With(wq) => self.engine.run_with(wq, &self.params)?,
            };
            self.cost = r.cost;
            self.state = DeferredState::Yielding(r.rows.into_iter());
        }
        match &mut self.state {
            DeferredState::Yielding(iter) => Ok(iter.next()),
            DeferredState::Pending | DeferredState::Done => Ok(None),
        }
    }

    fn cost(&self) -> i64 {
        self.cost
    }

    fn close(&mut self) {
        // The frozen snapshot is owned by `self.engine` and released on `Drop`; drop any pending query
        // + unread rows so a further `next_row` is a no-op (streaming.md §5, idempotent).
        self.query = None;
        self.state = DeferredState::Done;
    }
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
        Value::Float32(f) => RExpr::ConstFloat32(*f),
        Value::Float64(f) => RExpr::ConstFloat64(*f),
        Value::Bytea(b) => RExpr::ConstBytea(b.clone()),
        Value::Uuid(u) => RExpr::ConstUuid(*u),
        Value::Timestamp(m) => RExpr::ConstTimestamp(*m),
        Value::Timestamptz(m) => RExpr::ConstTimestamptz(*m),
        Value::Date(d) => RExpr::ConstDate(*d),
        Value::Interval(iv) => RExpr::ConstInterval(*iv),
        // A folded composite constant: fold each field and wrap in a ROW node so eval rebuilds the
        // `Value::Composite` (spec/design/composite.md).
        Value::Composite(fields) => RExpr::Row(fields.iter().map(value_to_rexpr).collect()),
        // A folded array constant — preserve its full shape (dims/lbounds) in a const node.
        Value::Array(arr) => RExpr::ConstArray(Box::new(arr.clone())),
        Value::Range(r) => RExpr::ConstRange(Box::new(r.clone())),
        Value::Json(s) => RExpr::ConstJson(s.clone()),
        Value::JsonPath(s) => RExpr::ConstJsonPath(s.clone()),
        Value::Jsonb(n) => RExpr::ConstJsonb(Box::new(n.clone())),
        // Poisoned (large-values.md §14): a folded subquery's projections are resolved values.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
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
        // A VALUES body is planned `parent = None`, so its values hold no outer reference of their
        // own; a folded-in subquery, however, may correlate to the target scope.
        QueryPlan::Values(vp) => vp
            .rows
            .iter()
            .flatten()
            .any(|e| rexpr_references_outer(e, depth)),
        // A nested `WITH` adds no correlation frame: its body is at the same depth, and the CTE
        // bodies are planned `parent = None` (they hold no outer reference), so only the body can
        // correlate to an enclosing scope (spec/design/cte.md §7).
        QueryPlan::With(wp) => query_plan_references_outer(&wp.body, depth),
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
        // A materialized ORDER BY expression may itself carry a correlated reference
        // (query.order_by_correlated): a subquery whose ONLY outer reference is in its ORDER BY is
        // still correlated and must re-execute per outer row (else its OuterColumn reads an empty
        // outer-row environment).
        || sp
            .order_exprs
            .iter()
            .any(|oe| rexpr_references_outer(oe, depth))
        // A set-returning relation's arguments may carry a correlated reference (an implicitly-
        // lateral SRF arg sees params / outer / an earlier sibling — functions.md §10, grammar.md
        // §44), which makes the enclosing query correlated, so it must NOT be folded once.
        || sp.rels.iter().any(|r| {
            r.srf
                .as_ref()
                .is_some_and(|srf| srf.args.iter().any(|a| rexpr_references_outer(a, depth)))
        })
        // A LATERAL derived table's body is one frame deeper; a reference in it back into this
        // query's outer (e.g. a nested lateral reaching a grandparent relation) counts here so the
        // enclosing item is correctly flagged correlated (spec/design/grammar.md §44).
        || sp
            .rels
            .iter()
            .any(|r| r.derived.as_ref().is_some_and(|d| query_plan_references_outer(d, depth + 1)))
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
        RExpr::Quantified { lhs, array, .. } => {
            rexpr_references_outer(lhs, depth) || rexpr_references_outer(array, depth)
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            rexpr_references_outer(inner, depth)
        }
        RExpr::Neg { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Not(x) => rexpr_references_outer(x, depth),
        RExpr::Casing { arg, .. } => rexpr_references_outer(arg, depth),
        RExpr::AtTimeZone { zone, value, .. } => {
            rexpr_references_outer(zone, depth) || rexpr_references_outer(value, depth)
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rexpr_references_outer(unit, depth)
                || rexpr_references_outer(value, depth)
                || zone
                    .as_ref()
                    .is_some_and(|z| rexpr_references_outer(z, depth))
        }
        RExpr::Extract { value, .. } => rexpr_references_outer(value, depth),
        RExpr::DateConvert { inner, .. } => rexpr_references_outer(inner, depth),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            rexpr_references_outer(lhs, depth) || rexpr_references_outer(rhs, depth)
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            rexpr_references_outer(base, depth) || rexpr_references_outer(arg, depth)
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rexpr_references_outer(a, depth) || rexpr_references_outer(b, depth)
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            rexpr_references_outer(l, depth) || rexpr_references_outer(r, depth)
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .any(|(c, r)| rexpr_references_outer(c, depth) || rexpr_references_outer(r, depth))
                || rexpr_references_outer(els, depth)
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::RangeFunc { args, .. }
        | RExpr::RegexFunc { args, .. }
        | RExpr::RangeCtor { args, .. }
        | RExpr::RangeOp { args, .. }
        | RExpr::RangeSetOp { args, .. }
        | RExpr::Variadic { args, .. }
        | RExpr::JsonBuild { args, .. }
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonPathFn { args, .. } => args.iter().any(|a| rexpr_references_outer(a, depth)),
        RExpr::JsonSqlFn { ctx, path, .. } => {
            rexpr_references_outer(ctx, depth) || rexpr_references_outer(path, depth)
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            fields.iter().any(|f| rexpr_references_outer(f, depth))
        }
        RExpr::Field { base, .. } => rexpr_references_outer(base, depth),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rexpr_references_outer(base, depth)
                || subscripts
                    .iter()
                    .flat_map(subscript_bounds)
                    .any(|e| rexpr_references_outer(e, depth))
        }
        RExpr::Column(_)
        | RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::ConstNull => false,
    }
}

/// The bound expressions of one resolved subscript spec (an index, or a slice's present
/// lower/upper bounds) — for the RExpr tree walkers.
fn subscript_bounds(s: &RSubscript) -> Vec<&RExpr> {
    match s {
        RSubscript::Index(i) => vec![i],
        RSubscript::Slice { lower, upper } => lower
            .iter()
            .chain(upper.iter())
            .map(|b| b.as_ref())
            .collect(),
    }
}

/// Collect the combined-row columns an expression **statically references** — the touched set
/// (cost.md §3 "The touched set"; large-values.md §14). Depth bookkeeping mirrors
/// `rexpr_references_outer`: walking the target plan's own clauses is depth 0 (a `Column`
/// touches); inside a nested subquery a `Column` indexes the subquery's own row (ignored) and an
/// `OuterColumn { level == depth }` is a correlated reference back into the target scope
/// (touches). Purely syntactic — a never-taken CASE branch still touches — so the set is
/// deterministic and cross-core identical (a §8 contract).
fn collect_touched(e: &RExpr, depth: usize, touched: &mut [bool]) {
    match e {
        RExpr::Column(i) => {
            // A `Column` index beyond the real columns is a SYNTHETIC slot (an aggregate or window
            // result, spec/design/window.md §5.1), not a table column — it touches no stored data,
            // so the bound check skips it rather than panicking.
            if depth == 0 && *i < touched.len() {
                touched[*i] = true;
            }
        }
        RExpr::OuterColumn { level, index } => {
            // A correlated reference into the scope we are collecting for (its frame is `depth` levels
            // up). The index is a slot in that target scope's combined row; bounds-checked like the
            // Column case. Callers collect at the depth matching the reference's level — a correlated
            // subquery at its nesting depth, a LATERAL SRF arg at depth 1 (its sibling frame).
            if *level == depth && depth > 0 && *index < touched.len() {
                touched[*index] = true;
            }
        }
        RExpr::Subquery { plan, lhs, .. } => {
            if let Some(l) = lhs {
                collect_touched(l, depth, touched);
            }
            collect_touched_plan(plan, depth + 1, touched);
        }
        RExpr::InValues { lhs, .. } => collect_touched(lhs, depth, touched),
        RExpr::Quantified { lhs, array, .. } => {
            collect_touched(lhs, depth, touched);
            collect_touched(array, depth, touched);
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            collect_touched(inner, depth, touched)
        }
        RExpr::Neg { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Not(x) => collect_touched(x, depth, touched),
        RExpr::Casing { arg, .. } => collect_touched(arg, depth, touched),
        RExpr::AtTimeZone { zone, value, .. } => {
            collect_touched(zone, depth, touched);
            collect_touched(value, depth, touched);
        }
        RExpr::DateTrunc { unit, value, zone } => {
            collect_touched(unit, depth, touched);
            collect_touched(value, depth, touched);
            if let Some(z) = zone {
                collect_touched(z, depth, touched);
            }
        }
        RExpr::Extract { value, .. } => collect_touched(value, depth, touched),
        RExpr::DateConvert { inner, .. } => collect_touched(inner, depth, touched),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            collect_touched(lhs, depth, touched);
            collect_touched(rhs, depth, touched);
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            collect_touched(base, depth, touched);
            collect_touched(arg, depth, touched);
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            collect_touched(a, depth, touched);
            collect_touched(b, depth, touched);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            collect_touched(l, depth, touched);
            collect_touched(r, depth, touched);
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Case { arms, els, .. } => {
            for (c, r) in arms {
                collect_touched(c, depth, touched);
                collect_touched(r, depth, touched);
            }
            collect_touched(els, depth, touched);
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::RangeFunc { args, .. }
        | RExpr::RegexFunc { args, .. }
        | RExpr::RangeCtor { args, .. }
        | RExpr::RangeOp { args, .. }
        | RExpr::RangeSetOp { args, .. }
        | RExpr::Variadic { args, .. }
        | RExpr::JsonBuild { args, .. }
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonPathFn { args, .. } => {
            for a in args {
                collect_touched(a, depth, touched);
            }
        }
        RExpr::JsonSqlFn { ctx, path, .. } => {
            collect_touched(ctx, depth, touched);
            collect_touched(path, depth, touched);
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            for f in fields {
                collect_touched(f, depth, touched);
            }
        }
        RExpr::Field { base, .. } => collect_touched(base, depth, touched),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            collect_touched(base, depth, touched);
            for e in subscripts.iter().flat_map(subscript_bounds) {
                collect_touched(e, depth, touched);
            }
        }
        RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::ConstNull => {}
    }
}

/// The number of grouping sets a single GROUP BY term expands to, saturating well below `usize::MAX`
/// so a huge `CUBE` cannot overflow the product before the `MAX_GROUPING_SETS` limit check.
fn group_item_set_count(item: &GroupItem) -> usize {
    match item {
        GroupItem::Set(_) => 1,
        GroupItem::Rollup(groups) => groups.len() + 1,
        // CUBE of n column groups is 2ⁿ; clamp the exponent so the shift can't overflow.
        GroupItem::Cube(groups) => {
            if groups.len() >= 20 {
                usize::MAX >> 1
            } else {
                1usize << groups.len()
            }
        }
        GroupItem::GroupingSets(elems) => elems
            .iter()
            .map(group_item_set_count)
            .fold(0usize, |a, c| a.saturating_add(c)),
    }
}

/// Expand a single GROUP BY term into its list of grouping sets, each a list of column `Expr`s
/// (`ROLLUP`/`CUBE`/`GROUPING SETS` and nesting — spec/design/aggregates.md §12). The per-set column
/// order is the textual order; the set order is deterministic and identical across cores (tests
/// compare the row multiset with `rowsort`).
fn expand_group_item(item: &GroupItem) -> Vec<Vec<&Expr>> {
    match item {
        GroupItem::Set(cols) => vec![cols.iter().collect()],
        // ROLLUP(g1..gn): the prefixes longest-first down to the empty set — n+1 sets.
        GroupItem::Rollup(groups) => (0..=groups.len())
            .rev()
            .map(|k| groups[..k].iter().flatten().collect())
            .collect(),
        // CUBE(g1..gn): every subset of the column groups — 2ⁿ sets (bit i = include group i).
        GroupItem::Cube(groups) => (0..(1usize << groups.len()))
            .map(|mask| {
                let mut s: Vec<&Expr> = Vec::new();
                for (i, g) in groups.iter().enumerate() {
                    if mask & (1usize << i) != 0 {
                        s.extend(g.iter());
                    }
                }
                s
            })
            .collect(),
        // GROUPING SETS(e1..en): the concatenation of each element's expansion.
        GroupItem::GroupingSets(elems) => elems.iter().flat_map(expand_group_item).collect(),
    }
}

/// Expand a whole GROUP BY clause into its grouping sets: the cross-product of the top-level terms'
/// expansions (`GROUP BY a, ROLLUP(b,c)` → `{(a,b,c),(a,b),(a)}`). An empty clause yields one empty
/// set (the whole-table grand total). Aborts `54001` if the expansion exceeds `MAX_GROUPING_SETS`.
fn expand_group_by(items: &[GroupItem]) -> Result<Vec<Vec<&Expr>>> {
    let mut total: usize = 1;
    for it in items {
        total = total.saturating_mul(group_item_set_count(it));
    }
    if total > MAX_GROUPING_SETS {
        return Err(EngineError::new(
            SqlState::StatementTooComplex,
            format!("too many grouping sets (the limit is {MAX_GROUPING_SETS})"),
        ));
    }
    let mut acc: Vec<Vec<&Expr>> = vec![Vec::new()];
    for it in items {
        let exp = expand_group_item(it);
        let mut next: Vec<Vec<&Expr>> = Vec::with_capacity(acc.len() * exp.len().max(1));
        for a in &acc {
            for s in &exp {
                let mut combined = a.clone();
                combined.extend(s.iter().copied());
                next.push(combined);
            }
        }
        acc = next;
    }
    Ok(acc)
}

/// The resolution of one `GROUP BY` grouping term (aggregates.md §15): either an input COLUMN at a
/// flat row index, or a general EXPRESSION to materialize (its resolved node + type + canonical AST).
pub(crate) enum GroupKeyResolved {
    Column(usize),
    Expr(RExpr, ResolvedType, Expr),
}

/// Resolve one `GROUP BY` grouping term to a column or a materialized expression (aggregates.md §15).
/// Classifies the term: a bare integer literal is a select-list ORDINAL (1-based; out of range
/// 42P10) whose target select item is then resolved as a term; otherwise it is a column / alias /
/// general expression (`resolve_group_named`).
fn resolve_group_term(
    scope: &Scope,
    term: &Expr,
    items: &SelectItems,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    // Only a *bare* integer literal is an ordinal — `GROUP BY 1`; `GROUP BY 1 + 1` is a constant
    // expression (PG). The parser folds a unary minus into the value, so a negative is just out of
    // range. The select list fixes the position count: `*` expands to the scope width.
    if let Expr::Literal(Literal::Int(n)) = term {
        let ncols = match items {
            SelectItems::All => scope.width() as i64,
            SelectItems::Items(its) => its.len() as i64,
        };
        if *n < 1 || *n > ncols {
            return Err(EngineError::new(
                SqlState::InvalidColumnReference,
                format!("GROUP BY position {n} is not in select list"),
            ));
        }
        let pos = (*n - 1) as usize;
        return match items {
            // `SELECT *` — the ordinal names the column at that scope position directly.
            SelectItems::All => Ok(GroupKeyResolved::Column(pos)),
            SelectItems::Items(its) => resolve_group_expr(scope, &its[pos].expr, params),
        };
    }
    resolve_group_named(scope, term, items, params)
}

/// Resolve a non-ordinal grouping term: a bare/qualified column, an output alias, or a general
/// expression (aggregates.md §15). A bare name resolves an INPUT column FIRST, then — only if there
/// is no such column — an output alias (PG's rule, the opposite of `ORDER BY`'s output-first rule).
fn resolve_group_named(
    scope: &Scope,
    term: &Expr,
    items: &SelectItems,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    match term {
        Expr::Column(name) => match scope.resolve_bare(name) {
            Ok(Resolved::Local(idx)) => Ok(GroupKeyResolved::Column(idx)),
            Ok(Resolved::Outer { .. }) => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "GROUP BY may not reference an outer query column",
            )),
            // No input column of this name: try an output alias (`SELECT a+b AS s … GROUP BY s`).
            // If none matches either, propagate the original 42703.
            Err(e) if e.state == SqlState::UndefinedColumn => {
                match order_alias_match(items, name, scope)? {
                    Some(aexpr) => resolve_group_expr(scope, aexpr, params),
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        },
        Expr::QualifiedColumn { qualifier, name } => {
            match scope.resolve_qualified(qualifier, name)? {
                Resolved::Local(idx) => Ok(GroupKeyResolved::Column(idx)),
                Resolved::Outer { .. } => Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "GROUP BY may not reference an outer query column",
                )),
            }
        }
        _ => resolve_group_expr(scope, term, params),
    }
}

/// Resolve a grouping expression (the target of an ordinal/alias, or a general `GROUP BY a+b`). A
/// plain column expression stays a COLUMN key (so the projection's bare-column path matches it);
/// anything else is MATERIALIZED — resolved against the input row with aggregates forbidden (an
/// aggregate in GROUP BY is 42803), its canonical AST kept for projection matching (aggregates.md §15).
fn resolve_group_expr(
    scope: &Scope,
    e: &Expr,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    match e {
        Expr::Column(name) => {
            if let Resolved::Local(idx) = scope.resolve_bare(name)? {
                return Ok(GroupKeyResolved::Column(idx));
            }
        }
        Expr::QualifiedColumn { qualifier, name } => {
            if let Resolved::Local(idx) = scope.resolve_qualified(qualifier, name)? {
                return Ok(GroupKeyResolved::Column(idx));
            }
        }
        _ => {}
    }
    let mut sub = AggCtx::Forbidden;
    let (rexpr, ty) = resolve(scope, e, None, &mut sub, params)?;
    Ok(GroupKeyResolved::Expr(rexpr, ty, e.clone()))
}

/// If `e` structurally matches a general-expression `GROUP BY` key in this aggregate context, return
/// that group's synthetic key slot (its master position) and resolved type (aggregates.md §15). Only
/// fires in `Collect` / `GroupedWindow`; an aggregate operand / FILTER resolves under `Forbidden`, so
/// a grouping expression there is correctly NOT remapped (it is a per-row value, not the group key).
fn match_group_expr(agg: &AggCtx, e: &Expr) -> Option<(usize, ResolvedType)> {
    let gke = match agg {
        AggCtx::Collect {
            group_key_exprs, ..
        }
        | AggCtx::GroupedWindow {
            group_key_exprs, ..
        } => group_key_exprs,
        _ => return None,
    };
    gke.iter().enumerate().find_map(|(p, gk)| match gk {
        Some((ge, ty)) if ge == e => Some((p, ty.clone())),
        _ => None,
    })
}

/// Compute a `GROUPING(args)` result for a group from the grouping set whose `mask` is given: bit
/// `(k-1-j)` of the result is bit `positions[j]` of `mask` (1 iff that column is grouped away in this
/// set). spec/design/aggregates.md §12.
fn grouping_value(positions: &[usize], mask: i64) -> i64 {
    let k = positions.len();
    let mut r: i64 = 0;
    for (j, &p) in positions.iter().enumerate() {
        let bit = (mask >> p) & 1;
        r |= bit << (k - 1 - j);
    }
    r
}

/// Rewrite placeholder column slots in `[base, 2·base)` (a window-result `WINDOW_RESULT_BASE + w` or
/// a `GROUPING(...)` `GROUPING_GS_BASE + g`) to their real synthetic slot `target + (slot - base)`,
/// once the grouped/windowed row layout is final (spec/design/window.md §5.1, aggregates.md §12/§21).
/// Each placeholder base is 2× the previous (`1<<28`, `1<<29`, `1<<30`) and a base's placeholder
/// count is far below that gap, so bounding the rewrite to `[base, 2·base)` keeps the bases isolated —
/// a window-result rebase no longer clobbers a `GROUPING()` placeholder (the two now COEXIST in a
/// GROUPING SETS + window query — aggregates.md §21).
fn rebase_placeholder_cols(e: &mut RExpr, from: usize, target: usize) {
    match e {
        RExpr::Column(i) => {
            if *i >= from && *i < from * 2 {
                *i = target + (*i - from);
            }
        }
        RExpr::Subquery { lhs, .. } => {
            if let Some(l) = lhs {
                rebase_placeholder_cols(l, from, target);
            }
        }
        RExpr::InValues { lhs, .. } => rebase_placeholder_cols(lhs, from, target),
        RExpr::Quantified { lhs, array, .. } => {
            rebase_placeholder_cols(lhs, from, target);
            rebase_placeholder_cols(array, from, target);
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            rebase_placeholder_cols(inner, from, target)
        }
        RExpr::Neg { operand, .. } => rebase_placeholder_cols(operand, from, target),
        RExpr::Not(x) => rebase_placeholder_cols(x, from, target),
        RExpr::Casing { arg, .. } => rebase_placeholder_cols(arg, from, target),
        RExpr::AtTimeZone { zone, value, .. } => {
            rebase_placeholder_cols(zone, from, target);
            rebase_placeholder_cols(value, from, target);
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rebase_placeholder_cols(unit, from, target);
            rebase_placeholder_cols(value, from, target);
            if let Some(z) = zone {
                rebase_placeholder_cols(z, from, target);
            }
        }
        RExpr::Extract { value, .. } => rebase_placeholder_cols(value, from, target),
        RExpr::DateConvert { inner, .. } => rebase_placeholder_cols(inner, from, target),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            rebase_placeholder_cols(lhs, from, target);
            rebase_placeholder_cols(rhs, from, target);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            rebase_placeholder_cols(l, from, target);
            rebase_placeholder_cols(r, from, target);
        }
        RExpr::IsNull { operand, .. } => rebase_placeholder_cols(operand, from, target),
        RExpr::Case { arms, els, .. } => {
            for (c, r) in arms {
                rebase_placeholder_cols(c, from, target);
                rebase_placeholder_cols(r, from, target);
            }
            rebase_placeholder_cols(els, from, target);
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::RangeFunc { args, .. }
        | RExpr::RegexFunc { args, .. }
        | RExpr::RangeCtor { args, .. }
        | RExpr::RangeOp { args, .. }
        | RExpr::RangeSetOp { args, .. }
        | RExpr::Variadic { args, .. }
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonPathFn { args, .. }
        | RExpr::JsonBuild { args, .. } => {
            for a in args {
                rebase_placeholder_cols(a, from, target);
            }
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            for f in fields {
                rebase_placeholder_cols(f, from, target);
            }
        }
        RExpr::Field { base, .. } => rebase_placeholder_cols(base, from, target),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rebase_placeholder_cols(base, from, target);
            for s in subscripts.iter_mut() {
                match s {
                    RSubscript::Index(i) => rebase_placeholder_cols(i, from, target),
                    RSubscript::Slice { lower, upper } => {
                        if let Some(l) = lower {
                            rebase_placeholder_cols(l, from, target);
                        }
                        if let Some(u) = upper {
                            rebase_placeholder_cols(u, from, target);
                        }
                    }
                }
            }
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            rebase_placeholder_cols(base, from, target);
            rebase_placeholder_cols(arg, from, target);
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rebase_placeholder_cols(a, from, target);
            rebase_placeholder_cols(b, from, target);
        }
        RExpr::JsonSqlFn { ctx, path, .. } => {
            rebase_placeholder_cols(ctx, from, target);
            rebase_placeholder_cols(path, from, target);
        }
        RExpr::IsJson { operand, .. } | RExpr::JsonCtor { operand, .. } => {
            rebase_placeholder_cols(operand, from, target)
        }
        RExpr::OuterColumn { .. }
        | RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::ConstNull => {}
    }
}

/// Walk a nested plan's expression surfaces for outer references back into the target scope —
/// the same five surfaces `select_plan_references_outer` checks (slot lists like group keys /
/// ORDER BY index the nested plan's own rows and can never reach outward).
fn collect_touched_plan(plan: &QueryPlan, depth: usize, touched: &mut [bool]) {
    match plan {
        QueryPlan::Select(sp) => {
            for j in &sp.joins {
                if let Some(on) = &j.on {
                    collect_touched(on, depth, touched);
                }
            }
            if let Some(f) = &sp.filter {
                collect_touched(f, depth, touched);
            }
            if let Some(h) = &sp.having {
                collect_touched(h, depth, touched);
            }
            for s in &sp.agg_specs {
                if let Some(op) = &s.operand {
                    collect_touched(op, depth, touched);
                }
            }
            for p in &sp.projections {
                collect_touched(p, depth, touched);
            }
            // A materialized ORDER BY expression and a set-returning relation's args / a LATERAL derived
            // body can each carry a correlated reference back into the target scope (the same surfaces
            // select_plan_references_outer checks — query.order_by_correlated, functions.md §10,
            // grammar.md §44). collect_touched_plan MUST cover every surface that function does, or an
            // outer column read only through one of them is left unfetched by the lazy/masked scan
            // (large-values.md §14) and the correlated subquery re-executes against NULL — a
            // memory-vs-disk divergence.
            for oe in &sp.order_exprs {
                collect_touched(oe, depth, touched);
            }
            for r in &sp.rels {
                if let Some(srf) = &r.srf {
                    for a in &srf.args {
                        collect_touched(a, depth, touched);
                    }
                }
                if let Some(derived) = &r.derived {
                    collect_touched_plan(derived, depth + 1, touched);
                }
            }
        }
        QueryPlan::SetOp(s) => {
            collect_touched_plan(&s.lhs, depth, touched);
            collect_touched_plan(&s.rhs, depth, touched);
        }
        QueryPlan::Values(vp) => {
            for row in &vp.rows {
                for e in row {
                    collect_touched(e, depth, touched);
                }
            }
        }
        // A nested `WITH`'s correlated references live in its body (the CTE bodies are `parent =
        // None`); recurse into the body at the same depth (spec/design/cte.md §7).
        QueryPlan::With(wp) => collect_touched_plan(&wp.body, depth, touched),
    }
}

/// Three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging one
/// `operator_eval` per element compared. An EMPTY list is `negated` (`x IN ()` = FALSE,
/// `x NOT IN ()` = TRUE) independent of `lv`. Otherwise: a positive match → TRUE; else a NULL
/// element (or NULL `lv`) → NULL (unknown); else FALSE. `NOT IN` is the Kleene negation. Shared
/// by the folded `InValues` node and the correlated `Subquery { In }` eval.
fn in_membership(lv: &Value, list: &[Value], negated: bool, m: &mut Meter) -> Result<Value> {
    if list.is_empty() {
        return Ok(Value::Bool(negated));
    }
    let mut any_match = false;
    let mut any_null = false;
    for v in list {
        m.charge(COSTS.operator_eval);
        // Each element comparison over a decimal pair charges its size-scaled decimal_work
        // (cost.md §3 "decimal_work"), like a Compare node.
        m.charge(COSTS.decimal_work * ((decimal_cmp_work(lv, v) - 1) as i64));
        m.guard()?;
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
    Ok(if negated { not3(&in_val) } else { in_val })
}

/// Build a binary-operator `Expr` node (used by the IN/BETWEEN desugar in `resolve`).
fn binary_expr(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// The `USING` column list a `NATURAL` join derives (spec/design/grammar.md §15): the column names
/// common to the LEFT relations of the join (`rels[seg..=k]`) and the right relation (`rels[k+1]`),
/// in LEFT order with each name taken once (its first occurrence). An empty result degenerates the
/// join to a `CROSS` join. (A merged column on the left keeps its underlying name, so a re-merge via
/// a NATURAL chain is found here too.)
fn natural_common_cols(rels: &[ScopeRel], seg: usize, k: usize) -> Vec<String> {
    let right = &rels[k + 1];
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in &rels[seg..=k] {
        for c in &r.table.columns {
            if seen.insert(c.name.to_ascii_lowercase())
                && right.table.column_index(&c.name).is_some()
            {
                out.push(c.name.clone());
            }
        }
    }
    out
}

/// The `(label, column-name)` of the relation owning a flat row index — used to synthesize a
/// `USING`/`NATURAL` join predicate's qualified column references (spec/design/grammar.md §15).
/// The index is known valid (resolution produced it), so the scan always finds an owner.
fn rel_of_index(rels: &[ScopeRel], idx: usize) -> (String, String) {
    for r in rels {
        let n = r.table.columns.len();
        if idx >= r.offset && idx < r.offset + n {
            return (
                r.label.clone(),
                r.table.columns[idx - r.offset].name.clone(),
            );
        }
    }
    unreachable!("USING merge index out of range")
}

// === Function registry (spec/design/extensibility.md §5) ============================
// Resolution for the named scalar functions and the aggregates is DATA-DRIVEN: instead of
// re-encoding the name set in hand-written `match`es (the old known-name gate + result-type
// match + name→variant match), it consults the generated catalog descriptor tables
// (`OPERATORS` rows with kind="function", and `AGGREGATES`) through the lookups below, keyed
// by (name, arg_families). The per-row KERNEL is still reached by id (`ScalarFunc` / `AggPlan`)
// and hand-written per core — §5 forbids codegenning the kernels. The only function-specific
// hand-written datum is `scalar_func_id` (name → kernel id); `registry_covers_catalog` (test)
// proves it total over the catalog. Host-registered functions would extend these lookups.

/// The argument family a resolved type satisfies, for matching a catalog `arg_families` slot.
/// `None` for NULL: an untyped NULL matches no *concrete* family — so `abs(NULL)` / `sum(NULL)`
/// find no overload (42883, the pre-registry behavior) — and only the wildcard "any" accepts it.
fn arg_family(t: &ResolvedType) -> Option<&'static str> {
    match t {
        ResolvedType::Int(_) => Some("integer"),
        ResolvedType::Decimal => Some("decimal"),
        ResolvedType::Float(_) => Some("float"),
        ResolvedType::Bool => Some("boolean"),
        ResolvedType::Text => Some("text"),
        ResolvedType::Bytea => Some("bytea"),
        ResolvedType::Uuid => Some("uuid"),
        ResolvedType::Timestamp => Some("timestamp"),
        ResolvedType::Timestamptz => Some("timestamptz"),
        ResolvedType::Date => Some("date"),
        ResolvedType::Interval => Some("interval"),
        ResolvedType::Json => Some("json"),
        ResolvedType::JsonPath => Some("jsonpath"),
        ResolvedType::Jsonb => Some("jsonb"),
        ResolvedType::Null => None,
        // A composite/array/range is no concrete built-in argument family this slice. (A range's
        // polymorphic `anyrange` family is matched separately by the range resolver — RF1.)
        ResolvedType::Composite(_) | ResolvedType::Array(_) | ResolvedType::Range(_) => None,
    }
}

/// Whether a resolved argument satisfies one catalog family slot. "any" accepts everything
/// (NULL included); a concrete family matches only its own type.
fn family_matches(slot: &str, t: &ResolvedType) -> bool {
    slot == "any" || arg_family(t) == Some(slot)
}

/// Whether `name` (case-insensitive) is a registered scalar function (catalog kind="function").
/// This is the data-driven replacement for the old hand-written known-name gate.
fn is_scalar_func_name(name: &str) -> bool {
    OPERATORS
        .iter()
        .any(|o| o.kind == "function" && o.name.eq_ignore_ascii_case(name))
}

/// Whether `name` (case-insensitive) is a VARIADIC scalar function (array-functions.md §12) — a
/// `kind="function"` row with `variadic = true` (`num_nulls`/`num_nonnulls`). Data-driven, so
/// adding a variadic row to the catalog wires it here without touching this gate.
fn is_variadic_func_name(name: &str) -> bool {
    OPERATORS
        .iter()
        .any(|o| o.kind == "function" && o.variadic && o.name.eq_ignore_ascii_case(name))
}

/// The matched scalar-function overload row for `name` over the resolved argument types: the
/// `kind="function"` catalog row whose `arg_families` agree by arity + per-slot family. `None`
/// ⇒ no overload (42883). `make_interval` resolves on its own named/defaulted path (§11).
fn lookup_scalar_overload(name: &str, arg_tys: &[ResolvedType]) -> Option<&'static OperatorDesc> {
    OPERATORS.iter().find(|o| {
        o.kind == "function"
            && o.name == name
            && o.arg_families.len() == arg_tys.len()
            && std::iter::zip(o.arg_families, arg_tys).all(|(slot, t)| family_matches(slot, t))
    })
}

/// The kernel id for scalar function `name` — the per-core hand-written half of the registry
/// (§5: the kernel is reached by id, never codegenned). Total over the catalog's function names
/// (`registry_covers_catalog` proves it); for Rust the id depends only on the name (one `Abs`
/// arm serves int/decimal/float; one `Round` arm serves float/decimal — the eval recovers the
/// overload from the operand value).
fn scalar_func_id(name: &str) -> ScalarFunc {
    match name {
        "abs" => ScalarFunc::Abs,
        "round" => ScalarFunc::Round,
        "ceil" => ScalarFunc::Ceil,
        "ceiling" => ScalarFunc::Ceil, // alias of ceil (same kernel)
        "floor" => ScalarFunc::Floor,
        "trunc" => ScalarFunc::Trunc,
        "sqrt" => ScalarFunc::Sqrt,
        "exp" => ScalarFunc::Exp,
        "ln" => ScalarFunc::Ln,
        // `log` is decimal-only (1-arg base-10 / 2-arg arbitrary-base); `log10` keeps its own id.
        "log" => ScalarFunc::Log,
        "log10" => ScalarFunc::Log10,
        // `power` is PG's name for `pow` (the documented name gap) — same kernel.
        "pow" | "power" => ScalarFunc::Pow,
        "sin" => ScalarFunc::Sin,
        "cos" => ScalarFunc::Cos,
        "tan" => ScalarFunc::Tan,
        "cbrt" => ScalarFunc::Cbrt,
        "pi" => ScalarFunc::Pi,
        "radians" => ScalarFunc::Radians,
        "degrees" => ScalarFunc::Degrees,
        "asin" => ScalarFunc::Asin,
        "acos" => ScalarFunc::Acos,
        "atan" => ScalarFunc::Atan,
        "atan2" => ScalarFunc::Atan2,
        "cot" => ScalarFunc::Cot,
        "sinh" => ScalarFunc::Sinh,
        "cosh" => ScalarFunc::Cosh,
        "tanh" => ScalarFunc::Tanh,
        "asinh" => ScalarFunc::Asinh,
        "acosh" => ScalarFunc::Acosh,
        "atanh" => ScalarFunc::Atanh,
        "sign" => ScalarFunc::Sign,
        "factorial" => ScalarFunc::Factorial,
        "scale" => ScalarFunc::Scale,
        "min_scale" => ScalarFunc::MinScale,
        "trim_scale" => ScalarFunc::TrimScale,
        "make_interval" => ScalarFunc::MakeInterval,
        // make_timestamp / make_timestamptz resolve on their own named/un-defaulted path (§11), like
        // make_interval; the name→kernel mapping is kept for the registry-coverage invariant.
        "make_timestamp" => ScalarFunc::MakeTimestamp,
        "make_timestamptz" => ScalarFunc::MakeTimestamptz,
        // uuid extractors + generators (functions.md §12, entropy.md §3). The generators are
        // volatile (drawn from the entropy seam at eval); the kernel id is still the name.
        "uuid_extract_version" => ScalarFunc::UuidExtractVersion,
        "uuid_extract_timestamp" => ScalarFunc::UuidExtractTimestamp,
        "uuidv4" => ScalarFunc::Uuidv4,
        "uuidv7" => ScalarFunc::Uuidv7,
        "now" => ScalarFunc::Now,
        "clock_timestamp" => ScalarFunc::ClockTimestamp,
        // Sequence value functions (sequences.md §4). nextval/setval MUTATE (write path); all but
        // lastval resolve their text argument to a catalog sequence at eval.
        "nextval" => ScalarFunc::Nextval,
        "currval" => ScalarFunc::Currval,
        "setval" => ScalarFunc::Setval,
        "lastval" => ScalarFunc::Lastval,
        // SessionState-variable read (spec/design/session.md §6.1): reads the session's variable map.
        "current_setting" => ScalarFunc::CurrentSetting,
        // json/jsonb processing functions (B1, json-sql-functions.md §2).
        "jsonb_typeof" => ScalarFunc::JsonbTypeof,
        "json_typeof" => ScalarFunc::JsonTypeof,
        "jsonb_array_length" => ScalarFunc::JsonbArrayLength,
        "json_array_length" => ScalarFunc::JsonArrayLength,
        "jsonb_strip_nulls" => ScalarFunc::JsonbStripNulls,
        "json_strip_nulls" => ScalarFunc::JsonStripNulls,
        "jsonb_pretty" => ScalarFunc::JsonbPretty,
        "to_jsonb" => ScalarFunc::ToJsonb,
        "to_json" => ScalarFunc::ToJson,
        "json_scalar" => ScalarFunc::JsonScalar,
        "json_serialize" => ScalarFunc::JsonSerialize,
        // string / text functions (string-functions.md). char_length/character_length are
        // SQL-standard aliases of length (same code-point-count kernel).
        "length" | "char_length" | "character_length" => ScalarFunc::Length,
        "octet_length" => ScalarFunc::OctetLength,
        "bit_length" => ScalarFunc::BitLength,
        "substr" => ScalarFunc::Substr,
        "left" => ScalarFunc::Left,
        "right" => ScalarFunc::Right,
        "lpad" => ScalarFunc::Lpad,
        "rpad" => ScalarFunc::Rpad,
        "btrim" => ScalarFunc::Btrim,
        "ltrim" => ScalarFunc::Ltrim,
        "rtrim" => ScalarFunc::Rtrim,
        "replace" => ScalarFunc::Replace,
        "translate" => ScalarFunc::Translate,
        "repeat" => ScalarFunc::Repeat,
        "reverse" => ScalarFunc::Reverse,
        "strpos" => ScalarFunc::Strpos,
        "split_part" => ScalarFunc::SplitPart,
        "starts_with" => ScalarFunc::StartsWith,
        "ascii" => ScalarFunc::Ascii,
        "chr" => ScalarFunc::Chr,
        "initcap" => ScalarFunc::Initcap,
        "to_hex" => ScalarFunc::ToHex,
        "encode" => ScalarFunc::Encode,
        "decode" => ScalarFunc::Decode,
        "quote_literal" => ScalarFunc::QuoteLiteral,
        "quote_ident" => ScalarFunc::QuoteIdent,
        "quote_nullable" => ScalarFunc::QuoteNullable,
        _ => unreachable!("scalar_func_id: {name} is not a catalog function"),
    }
}

/// The kernel id for VARIADIC function `name` (array-functions.md §12). Total over the catalog's
/// variadic-function names (`is_variadic_func_name` gates the call; `registry_covers_catalog` proves
/// coverage).
fn variadic_func_id(name: &str) -> VariadicFunc {
    match name {
        "num_nulls" => VariadicFunc::NumNulls,
        "num_nonnulls" => VariadicFunc::NumNonnulls,
        _ => unreachable!("variadic_func_id: {name} is not a catalog variadic function"),
    }
}

/// The result `ScalarType` of a scalar function from its catalog `result` code (functions.md §9):
/// "promoted" = the (single) operand's own type; otherwise the code is a literal scalar-type id
/// (e.g. "decimal", "f64", "interval", "i16", "timestamptz", "uuid") naming the result.
fn scalar_result_type(code: &str, arg_tys: &[ResolvedType]) -> ScalarType {
    if code == "promoted" {
        return resolved_scalar_type(&arg_tys[0]);
    }
    ScalarType::from_name(code)
        .unwrap_or_else(|| unreachable!("scalar_result_type: unknown result code {code}"))
}

/// The concrete `ScalarType` carried by a numeric resolved type (for the "promoted" /
/// "same_as_input" result rules). Only reached for the numeric families those rules admit.
fn resolved_scalar_type(t: &ResolvedType) -> ScalarType {
    match t {
        ResolvedType::Int(it) => *it,
        ResolvedType::Float(ft) => *ft,
        ResolvedType::Decimal => ScalarType::Decimal,
        _ => unreachable!("resolved_scalar_type: non-numeric operand"),
    }
}

/// The `ScalarType` of a *scalar* resolved type, or `None` for a container/null type (composite /
/// array / range / json / null). Total over every `ResolvedType`. Used by the element-wise
/// array→array cast resolver (spec/design/array.md §7) to decide whether the source element type is
/// a scalar with an admitted [`scalar_pair_castable`] cast to the target element scalar.
fn resolved_to_scalar(t: &ResolvedType) -> Option<ScalarType> {
    Some(match t {
        ResolvedType::Int(s) | ResolvedType::Float(s) => *s,
        ResolvedType::Decimal => ScalarType::Decimal,
        ResolvedType::Bool => ScalarType::Bool,
        ResolvedType::Text => ScalarType::Text,
        ResolvedType::Bytea => ScalarType::Bytea,
        ResolvedType::Uuid => ScalarType::Uuid,
        ResolvedType::Timestamp => ScalarType::Timestamp,
        ResolvedType::Timestamptz => ScalarType::Timestamptz,
        ResolvedType::Date => ScalarType::Date,
        ResolvedType::Interval => ScalarType::Interval,
        ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Null => return None,
    })
}

// === Polymorphic array-function resolution (spec/design/array-functions.md §2) ======
// The `anyarray`/`anyelement` pseudo-families are NOT real families (arg_family returns None for
// an array), so the generic `lookup_scalar_overload` cannot match an array function. These helpers
// add the unification: one type variable ELEM, bound from an `anyarray` slot's element type and an
// `anyelement` slot's type, by structural equality (`ResolvedType: Eq`), and read back into the
// reserved result codes `anyarray` (= ELEM[]) and `anyelement` (= ELEM).

/// Whether `name` (case-insensitive) is a polymorphic array function — a `kind="function"`
/// catalog row whose `arg_families` mention `anyarray`/`anyelement`. Data-driven, so adding an
/// array-function row to the catalog wires it here without touching this gate.
fn is_array_func_name(name: &str) -> bool {
    OPERATORS.iter().any(|o| {
        o.kind == "function"
            && o.name.eq_ignore_ascii_case(name)
            && o.arg_families
                .iter()
                .any(|f| *f == "anyarray" || *f == "anyelement")
    })
}

/// The kernel id for array function `name` (each name is single-arity, so the name alone selects
/// the kernel). Total over the catalog's array-function names (`is_array_func_name` gates the call).
fn array_func_id(name: &str) -> ArrayFunc {
    match name {
        "array_ndims" => ArrayFunc::ArrayNdims,
        "array_length" => ArrayFunc::ArrayLength,
        "array_lower" => ArrayFunc::ArrayLower,
        "array_upper" => ArrayFunc::ArrayUpper,
        "cardinality" => ArrayFunc::Cardinality,
        "array_dims" => ArrayFunc::ArrayDims,
        "array_append" => ArrayFunc::ArrayAppend,
        "array_prepend" => ArrayFunc::ArrayPrepend,
        "array_cat" => ArrayFunc::ArrayCat,
        "array_remove" => ArrayFunc::ArrayRemove,
        "array_replace" => ArrayFunc::ArrayReplace,
        "array_position" => ArrayFunc::ArrayPosition,
        "array_positions" => ArrayFunc::ArrayPositions,
        "array_to_json" => ArrayFunc::ArrayToJson,
        _ => unreachable!("array_func_id: {name} is not a catalog array function"),
    }
}

/// Bind/check the type variable ELEM against a concrete type `x`: bind if unbound, else require
/// structural equality. `false` ⇒ a conflict (e.g. `array_cat(i32[], text[])`) — the overload
/// does not match. An untyped `NULL` operand never reaches here (the caller defers it).
fn unify_elem(elem: &mut Option<ResolvedType>, x: &ResolvedType) -> bool {
    match elem {
        None => {
            *elem = Some(x.clone());
            true
        }
        Some(e) => e == x,
    }
}

/// Match an overload's `arg_families` (which may contain `anyarray`/`anyelement`) against the
/// resolved argument types, returning the bound ELEM (`Some(None)` = matched but every polymorphic
/// arg was an untyped NULL, so ELEM is undeterminable; `None` = no match). Three passes: `anyarray`
/// slots first (they definitively bind ELEM := the element type), then `anyelement` (which may
/// precede its binding array — `array_prepend`), then the concrete family slots.
fn match_poly(slots: &[&str], tys: &[ResolvedType]) -> Option<Option<ResolvedType>> {
    let mut elem: Option<ResolvedType> = None;
    for (slot, t) in std::iter::zip(slots, tys) {
        if *slot == "anyarray" {
            match t {
                ResolvedType::Array(e) => {
                    if !unify_elem(&mut elem, e) {
                        return None;
                    }
                }
                ResolvedType::Null => {} // untyped NULL — defer, contributes no binding
                _ => return None,        // a non-array where anyarray is required
            }
        }
    }
    // `anyrange` binds ELEM := the range's element type, like `anyarray` (both definitive, before
    // `anyelement`) — range-functions.md §1.
    for (slot, t) in std::iter::zip(slots, tys) {
        if *slot == "anyrange" {
            match t {
                ResolvedType::Range(e) => {
                    if !unify_elem(&mut elem, e) {
                        return None;
                    }
                }
                ResolvedType::Null => {} // untyped NULL — defer, contributes no binding
                _ => return None,        // a non-range where anyrange is required
            }
        }
    }
    for (slot, t) in std::iter::zip(slots, tys) {
        if *slot == "anyelement" {
            match t {
                ResolvedType::Null => {} // untyped NULL — defer
                _ => {
                    if !unify_elem(&mut elem, t) {
                        return None;
                    }
                }
            }
        }
    }
    for (slot, t) in std::iter::zip(slots, tys) {
        if *slot != "anyarray"
            && *slot != "anyrange"
            && *slot != "anyelement"
            && !family_matches(slot, t)
        {
            return None;
        }
    }
    Some(elem)
}

/// The result `ResolvedType` of an array function from its catalog `result` code and the bound
/// ELEM: `anyarray` → `ELEM[]`, `anyelement` → `ELEM` (both 42P18 if ELEM is undeterminable — every
/// polymorphic arg was an untyped NULL); any other code is a concrete scalar id (`i32`, `text`).
fn poly_result_type(code: &str, elem: &Option<ResolvedType>) -> Result<ResolvedType> {
    match code {
        "anyarray" => match elem {
            Some(e) => Ok(ResolvedType::Array(Box::new(e.clone()))),
            None => Err(indeterminate_poly()),
        },
        "anyrange" => match elem {
            Some(e) => Ok(ResolvedType::Range(Box::new(e.clone()))),
            None => Err(indeterminate_poly()),
        },
        "anyelement" => match elem {
            Some(e) => Ok(e.clone()),
            None => Err(indeterminate_poly()),
        },
        // A concrete array result `<scalar>[]` (array_positions → "i32[]"): the element type is
        // fixed (independent of ELEM), so the result is `Array(scalar)` (array-functions.md §8).
        c if c.ends_with("[]") => {
            let base = &c[..c.len() - 2];
            let st = ScalarType::from_name(base)
                .unwrap_or_else(|| unreachable!("poly_result_type: unknown array element {base}"));
            Ok(ResolvedType::Array(Box::new(resolved_type_of(st))))
        }
        _ => Ok(resolved_type_of(
            ScalarType::from_name(code)
                .unwrap_or_else(|| unreachable!("poly_result_type: unknown result code {code}")),
        )),
    }
}

/// The 42P18 raised when an array function's polymorphic type cannot be determined because every
/// polymorphic argument was an untyped `NULL` (`array_append(NULL, NULL)` — array-functions.md §5).
fn indeterminate_poly() -> EngineError {
    EngineError::new(
        SqlState::IndeterminateDatatype,
        "could not determine polymorphic type because input has type unknown",
    )
}

/// The element type's `ScalarType`, for the literal-adaptation hint (array-functions.md §2): the
/// bound array element type is threaded back as the `ctx` when re-resolving the polymorphic args,
/// so a bare integer/decimal literal element adapts (with range-checking) to that type — e.g.
/// `array_append(i32[], 40)` adapts `40` to `i32`. `None` for a composite/array/NULL element.
fn elem_scalar_hint(t: &ResolvedType) -> Option<ScalarType> {
    match t {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        ResolvedType::Interval => Some(ScalarType::Interval),
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => None,
    }
}

/// Resolve a polymorphic array function call (array-functions.md §3): resolve the arguments, unify
/// ELEM across the `anyarray`/`anyelement` slots to pick the overload (42883 on no match), and
/// compute the result type from the matched `result` code. The kernel id is the name; NULL handling
/// (the introspectors propagate, the builders are non-strict) lives in the eval kernel.
///
/// Two passes (§2): pass 1 resolves the arguments with no hint to discover the array's element
/// type; if that element is a scalar, pass 2 re-resolves the polymorphic-slot arguments with it as
/// the `ctx`, so an untyped literal element (or an `ARRAY[…]` constructor argument) adapts to the
/// array's element type — `array_append(i32[], 40)` and `array_cat(i32[], ARRAY[7,8])` both
/// land on `i32`, with a range check on the literal. (The concrete `integer` dimension slot of
/// `array_length`/`lower`/`upper` keeps its pass-1 resolution.)
fn resolve_array_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Each array-function name is single-overload; find its row by (name, arity). A wrong argument
    // count matches no overload (42883), exactly as a missing scalar overload does.
    let desc = OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == args.len())
        .ok_or_else(|| no_func_overload(name))?;
    let slots = desc.arg_families;

    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        let (r, t) = resolve(scope, a, None, agg, params)?;
        rargs.push(r);
        tys.push(t);
    }
    // Pass 2: adapt the polymorphic args to the array's element type, if it is a scalar.
    let hint = slots
        .iter()
        .zip(tys.iter())
        .find_map(|(slot, t)| match (*slot, t) {
            ("anyarray", ResolvedType::Array(e)) => elem_scalar_hint(e),
            _ => None,
        });
    if let Some(s) = hint {
        for (i, slot) in slots.iter().enumerate() {
            if *slot == "anyarray" || *slot == "anyelement" {
                let (r, t) = resolve(scope, &args[i], Some(s), agg, params)?;
                rargs[i] = r;
                tys[i] = t;
            }
        }
    }
    let elem = match_poly(slots, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = poly_result_type(desc.result, &elem)?;
    Ok((
        RExpr::ArrayFunc {
            func: array_func_id(name),
            args: rargs,
        },
        result,
    ))
}

/// Whether `name` (case-insensitive) is a polymorphic range function — a `kind="function"` catalog
/// row whose `arg_families` mention `anyrange` (range-functions.md §1). Data-driven, so a new
/// range-function row wires here without touching this gate.
fn is_range_func_name(name: &str) -> bool {
    OPERATORS.iter().any(|o| {
        o.kind == "function"
            && o.name.eq_ignore_ascii_case(name)
            && o.arg_families.iter().any(|f| *f == "anyrange")
    })
}

/// The kernel id for range accessor `name` (each is single-arity, so the name selects the kernel).
/// Total over the catalog's range-function names (`is_range_func_name` gates the call).
fn range_func_id(name: &str) -> RangeFunc {
    match name {
        "lower" => RangeFunc::Lower,
        "upper" => RangeFunc::Upper,
        "isempty" => RangeFunc::IsEmpty,
        "lower_inc" => RangeFunc::LowerInc,
        "upper_inc" => RangeFunc::UpperInc,
        "lower_inf" => RangeFunc::LowerInf,
        "upper_inf" => RangeFunc::UpperInf,
        _ => unreachable!("range_func_id: {name} is not a catalog range function"),
    }
}

/// Resolve a polymorphic range accessor over the `anyrange` pseudo-family (range-functions.md §1).
/// Simpler than [`resolve_array_func`] — the accessors take a single `anyrange` arg with no
/// `anyelement` arg, so there is no element-hint literal adaptation. `lower`/`upper` resolve to ELEM
/// (the bound type), the rest to boolean.
fn resolve_range_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let desc = OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == args.len())
        .ok_or_else(|| no_func_overload(name))?;
    let slots = desc.arg_families;

    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        let (r, t) = resolve(scope, a, None, agg, params)?;
        rargs.push(r);
        tys.push(t);
    }
    let elem = match_poly(slots, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = poly_result_type(desc.result, &elem)?;
    // `range_merge(anyrange, anyrange) → anyrange` is a SET operation (= union, non-strict), not a
    // scalar accessor: emit the shared `RangeSetOp` node (range-functions.md §4). `poly_result_type`
    // already raised 42P18 if the element was indeterminate (both args untyped NULL), so `elem` is
    // bound here.
    if name == "range_merge" {
        return Ok((
            RExpr::RangeSetOp {
                op: RangeSetOp::Merge,
                args: rargs,
            },
            result,
        ));
    }
    Ok((
        RExpr::RangeFunc {
            func: range_func_id(name),
            args: rargs,
        },
        result,
    ))
}

/// Resolve `lower`/`upper`, overloaded across the range accessors and the text casing functions
/// (functions.md §9, collation.md §16). The single argument resolves once (offering `text` as the
/// literal-adaptation hint, so a bare NULL / untyped `$1` adapts to text — the common case; a typed
/// range expression keeps its range type and ignores the scalar hint). A **text/NULL** argument folds
/// case (`RExpr::Casing`, result `text`); a **range** argument is the bound accessor (`RExpr::RangeFunc`,
/// result the range's element type); anything else is `42883` (no overload).
fn resolve_lower_upper(
    scope: &Scope,
    name: &str, // "lower" | "upper", already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 1 {
        return Err(no_func_overload(name));
    }
    let (r, t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    match t {
        ResolvedType::Text | ResolvedType::Null => Ok((
            RExpr::Casing {
                upper: name == "upper",
                arg: Box::new(r),
            },
            ResolvedType::Text,
        )),
        ResolvedType::Range(elem) => Ok((
            RExpr::RangeFunc {
                func: range_func_id(name),
                args: vec![r],
            },
            *elem, // lower(anyrange)/upper(anyrange) return the element type
        )),
        _ => Err(no_func_overload(name)),
    }
}

/// Resolve `timezone(zone, value)` — the desugar of `value AT TIME ZONE zone` (timezones.md §6).
/// `zone` must be text (else `42804`); the result family is the OTHER timestamp family of `value`:
/// `timestamptz` → `timestamp` (render the instant locally) and `timestamp` → `timestamptz`
/// (interpret the wall clock in the zone). Any other `value` family — or an untyped/NULL value, which
/// cannot pick an overload — is `42883`.
fn resolve_timezone(
    scope: &Scope,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 2 {
        return Err(no_func_overload("timezone"));
    }
    // args[0] = zone (text), args[1] = value (timestamp/timestamptz). The AT TIME ZONE desugar puts
    // the zone first, matching PostgreSQL's `timezone(text, timestamptz)` signature.
    let (zone_r, zone_t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    let (value_r, value_t) = resolve(scope, &args[1], None, agg, params)?;
    // A non-text zone, or a non-timestamp value, is `42883` — PG resolves AT TIME ZONE via function
    // overload (`timezone(text, timestamptz)` / `timezone(text, timestamp)`), so any other arg pair
    // is "no such function" (PG-matching, oracle-pinned), not a datatype_mismatch. A NULL zone is
    // allowed (it propagates to NULL at eval).
    let zone_ok = matches!(zone_t, ResolvedType::Text | ResolvedType::Null);
    let (to_timestamptz, result) = match (zone_ok, value_t) {
        (true, ResolvedType::Timestamptz) => (false, ResolvedType::Timestamp),
        (true, ResolvedType::Timestamp) => (true, ResolvedType::Timestamptz),
        _ => return Err(no_func_overload("timezone")),
    };
    Ok((
        RExpr::AtTimeZone {
            zone: Box::new(zone_r),
            value: Box::new(value_r),
            to_timestamptz,
        },
        result,
    ))
}

/// Resolve `date_trunc(unit, value[, zone])` (timezones.md §9.1). `unit` is text (a runtime value,
/// validated at eval); `value` is `timestamp` / `timestamptz` / `interval`; the optional `zone` (text)
/// is the 3-arg form, valid **only** for a `timestamptz` value. The result family is the `value`
/// family. A non-text unit/zone, a non-datetime value, or the 3-arg form on a non-`timestamptz` value
/// is `42883` (no such overload — PG-matching; a `date` value also has no overload, jed having no
/// implicit `date`→`timestamp` cast).
fn resolve_date_trunc(
    scope: &Scope,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 2 && args.len() != 3 {
        return Err(no_func_overload("date_trunc"));
    }
    let (unit_r, unit_t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    let (value_r, value_t) = resolve(scope, &args[1], None, agg, params)?;
    if !matches!(unit_t, ResolvedType::Text | ResolvedType::Null) {
        return Err(no_func_overload("date_trunc"));
    }
    let result = match value_t {
        ResolvedType::Timestamp | ResolvedType::Timestamptz | ResolvedType::Interval => value_t,
        _ => return Err(no_func_overload("date_trunc")),
    };
    let zone = if args.len() == 3 {
        // The 3-arg form is `date_trunc(text, timestamptz, text)` only (PG): a 3-arg call on a
        // timestamp/interval value is "no such function".
        if !matches!(result, ResolvedType::Timestamptz) {
            return Err(no_func_overload("date_trunc"));
        }
        let (zone_r, zone_t) = resolve(scope, &args[2], Some(ScalarType::Text), agg, params)?;
        if !matches!(zone_t, ResolvedType::Text | ResolvedType::Null) {
            return Err(no_func_overload("date_trunc"));
        }
        Some(Box::new(zone_r))
    } else {
        None
    };
    Ok((
        RExpr::DateTrunc {
            unit: Box::new(unit_r),
            value: Box::new(value_r),
            zone,
        },
        result,
    ))
}

/// Whether `name` (case-insensitive) is a range CONSTRUCTOR call (range-functions.md §2): a call
/// whose name is a range type name or alias (`i32range`/`int4range`/`numrange`/…). The constructor
/// functions are the only ones whose name is a range type name, so `range::range_by_name` resolving
/// is exactly the gate — data-driven over the RANGES table, no hand-written name list.
fn is_range_ctor_name(name: &str) -> bool {
    crate::range::range_by_name(name).is_some()
}

/// Whether a bound argument of resolved type `t` is assignable to range element `elem`, mirroring
/// the `store_value` coercions the kernel will apply (range-functions.md §2): a NULL is an infinite
/// bound (always ok); an integer adapts to an integer (range-checked) or decimal element; a decimal
/// to a decimal element; an already-temporal value to its own element; and a string literal/text to
/// a temporal element (parsed at eval). Anything else is no overload (42883).
fn range_bound_assignable(t: &ResolvedType, elem: ScalarType) -> bool {
    match t {
        ResolvedType::Null => true,
        ResolvedType::Int(_) => elem.is_integer() || elem.is_decimal(),
        ResolvedType::Decimal => elem.is_decimal(),
        ResolvedType::Timestamp => elem.is_timestamp(),
        ResolvedType::Timestamptz => elem.is_timestamptz(),
        ResolvedType::Date => elem.is_date(),
        ResolvedType::Text => elem.is_timestamp() || elem.is_timestamptz() || elem.is_date(),
        _ => false,
    }
}

/// Resolve a range constructor call (`i32range(lo, hi[, bounds])` and the five siblings, plus the
/// `int4range`/`int8range` aliases — range-functions.md §2). The target range type comes from the
/// call name (`range_by_name`, alias-aware); the result type is fixed (concrete), not polymorphic.
/// Each bound resolves with the element scalar as the literal-adaptation context (so `1` adapts to
/// the element width, `'2024-01-01'` to a date), then is type-checked assignable to the element; the
/// optional third argument is the bounds-flags TEXT. The kernel ([`eval_range_ctor`]) does the
/// element coercion (assignment-style, 22003), the flags parse (42601 / 22000), and `finalize`.
fn resolve_range_ctor(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let desc = crate::range::range_by_name(name).expect("is_range_ctor_name gated the call");
    let elem = crate::range::element_scalar(desc);
    // Only the 2-arg (lo, hi) and 3-arg (lo, hi, bounds) overloads exist.
    if args.len() != 2 && args.len() != 3 {
        return Err(no_func_overload(name));
    }
    let mut rargs = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        if i < 2 {
            // A bound: offer the element scalar as the literal-adaptation hint, then check the
            // resolved type is assignable to the element (else no overload).
            let (r, t) = resolve(scope, a, Some(elem), agg, params)?;
            if !range_bound_assignable(&t, elem) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        } else {
            // The bounds-flags argument: TEXT (a NULL is allowed at resolve — the kernel traps it
            // 22000 at eval, matching PG "flags argument must not be null").
            let (r, t) = resolve(scope, a, None, agg, params)?;
            if !matches!(t, ResolvedType::Text | ResolvedType::Null) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    }
    Ok((
        RExpr::RangeCtor { elem, args: rargs },
        ResolvedType::Range(Box::new(resolved_type_of(elem))),
    ))
}

/// Whether aggregate `surface` (case-insensitive) has a `COUNT(*)`-style star overload — only
/// COUNT does. The data-driven replacement for the special-cased `_ if star` arm.
fn aggregate_has_star(surface: &str) -> bool {
    AGGREGATES
        .iter()
        .any(|a| a.surface.eq_ignore_ascii_case(surface) && a.arg == "star")
}

/// The matched aggregate overload row for `surface` over a single operand of resolved type `t`:
/// the `arg="expr"` catalog row whose lone `arg_families` slot matches. `None` ⇒ no overload
/// (42883, e.g. `SUM(text)`). MIN/MAX/COUNT take "any" (NULL included); SUM/AVG a numeric family.
fn lookup_aggregate_overload(surface: &str, t: &ResolvedType) -> Option<&'static AggregateDesc> {
    AGGREGATES.iter().find(|a| {
        a.surface.eq_ignore_ascii_case(surface)
            && a.arg == "expr"
            && a.arg_families.len() == 1
            && family_matches(a.arg_families[0], t)
    })
}

/// The runtime plan + result type for an aggregate over operand type `t`, from the matched
/// overload's `surface` + catalog `result` code (the PG widening — aggregates.md §3). The plan
/// is the aggregate's kernel id (fold/finalize switch on it); selecting it from the registered
/// `result` code keeps the name gate + overload validation data-driven while the kernel stays
/// hand-written (§5). `surface` is the lowercased call name; `result` the matched row's code.
fn aggregate_plan(surface: &str, result: &str, t: &ResolvedType) -> (AggPlan, ResolvedType) {
    match (surface, result) {
        ("count", _) => (AggPlan::Count, ResolvedType::Int(ScalarType::Int64)),
        // SUM(i16|i32) → i64; SUM(i64) → decimal (PG widening).
        ("sum", "sum_widen") => match t {
            ResolvedType::Int(it) if *it == ScalarType::Int64 => {
                (AggPlan::SumDecimal, ResolvedType::Decimal)
            }
            ResolvedType::Int(_) => (AggPlan::SumInt, ResolvedType::Int(ScalarType::Int64)),
            _ => unreachable!("sum_widen matches only the integer family"),
        },
        ("sum", "decimal") => (AggPlan::SumDecimal, ResolvedType::Decimal),
        // SUM(float)/AVG(float) → SAME width (the canonical-order fold — float.md §7).
        ("sum", "same_as_input") => {
            let ft = resolved_scalar_type(t);
            (AggPlan::SumFloat(ft), ResolvedType::Float(ft))
        }
        ("avg", "decimal") => (AggPlan::Avg, ResolvedType::Decimal),
        ("avg", "same_as_input") => {
            let ft = resolved_scalar_type(t);
            (AggPlan::AvgFloat(ft), ResolvedType::Float(ft))
        }
        // MIN/MAX accept any ordered scalar; the result is the argument's own type.
        ("min", "same_as_input") => (AggPlan::Min, t.clone()),
        ("max", "same_as_input") => (AggPlan::Max, t.clone()),
        // json/jsonb array aggregates (B4). `compact` is the json (vs jsonb) render; `_strict`
        // skips a NULL input. The result type is json/jsonb (the catalog `result` code).
        ("jsonb_agg", "jsonb") => (
            AggPlan::JsonAgg {
                compact: false,
                strict: false,
            },
            ResolvedType::Jsonb,
        ),
        ("json_agg", "json") => (
            AggPlan::JsonAgg {
                compact: true,
                strict: false,
            },
            ResolvedType::Json,
        ),
        ("jsonb_agg_strict", "jsonb") => (
            AggPlan::JsonAgg {
                compact: false,
                strict: true,
            },
            ResolvedType::Jsonb,
        ),
        ("json_agg_strict", "json") => (
            AggPlan::JsonAgg {
                compact: true,
                strict: true,
            },
            ResolvedType::Json,
        ),
        _ => unreachable!("aggregate_plan: unhandled ({surface}, {result})"),
    }
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
    distinct: bool,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        // Forbidden (WHERE/JOIN ON/plain projection) and Window (a plain window query's projection)
        // both reject a bare aggregate here — 42803.
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    let mut sub = AggCtx::Forbidden;
    // json[b]_object_agg[_unique] take TWO operands (key, value) — resolve both and encode as a Row
    // operand for the single-operand aggregate framework (the fold splits the composite back out).
    if let Some((json, unique)) = object_agg_classify(name) {
        if star || args.len() != 2 {
            return Err(no_agg_overload(name));
        }
        let (rk, _kt) = resolve(scope, &args[0], None, &mut sub, params)?;
        let (rv, _vt) = resolve(scope, &args[1], None, &mut sub, params)?;
        let operand = RExpr::Row(vec![rk, rv]);
        let plan = AggPlan::JsonObjectAgg { json, unique };
        let result = if json {
            ResolvedType::Json
        } else {
            ResolvedType::Jsonb
        };
        // object_agg never carries DISTINCT/FILTER (the 2-arg key/value shape predates them and the
        // surface does not wire them — B4); both default off, matching Go's zero-valued aggSpec. It
        // collects into `Collect` or `GroupedWindow` (a grouped query that also windows), exactly like
        // any aggregate (spec/design/window.md §5.1).
        return match agg {
            AggCtx::Collect {
                group_keys, specs, ..
            } => {
                let slot = group_keys.len() + specs.len();
                specs.push(AggSpec {
                    plan,
                    operand: Some(operand),
                    distinct: false,
                    filter: None,
                    osa: None,
                    hypo: None,
                });
                Ok((RExpr::Column(slot), result))
            }
            AggCtx::GroupedWindow {
                group_keys,
                agg_specs,
                ..
            } => {
                let slot = group_keys.len() + agg_specs.len();
                agg_specs.push(AggSpec {
                    plan,
                    operand: Some(operand),
                    distinct: false,
                    filter: None,
                    osa: None,
                    hypo: None,
                });
                Ok((RExpr::Column(slot), result))
            }
            _ => unreachable!("an aggregate in a non-collecting context is handled above"),
        };
    }
    let (plan, operand, result) = if star {
        // Only COUNT has a star overload (aggregates.md §3); `SUM(*)` etc. is a syntax error.
        if !aggregate_has_star(name) {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        (
            AggPlan::CountStar,
            None,
            ResolvedType::Int(ScalarType::Int64),
        )
    } else {
        // One operand, resolved in a fresh Forbidden sub-context. The registry validates the
        // (surface, operand-family) overload exists (else 42883) and yields its result code; the
        // plan + result type follow from it (the PG widening).
        let arg = expect_arg(args)?;
        // An aggregate's argument may not contain a window function (PG 42803 — window.md §7): the
        // window stage runs AFTER aggregation, so a window result cannot be folded into an aggregate.
        if expr_has_window(arg) {
            return Err(EngineError::new(
                SqlState::GroupingError,
                "aggregate function calls cannot contain window function calls",
            ));
        }
        let (r, t) = resolve(scope, arg, None, &mut sub, params)?;
        let desc = lookup_aggregate_overload(name, &t).ok_or_else(|| no_agg_overload(name))?;
        let (plan, result) = aggregate_plan(name, desc.result, &t);
        (plan, Some(r), result)
    };
    // FILTER (WHERE cond): resolve the per-row predicate against the input row with aggregates
    // FORBIDDEN — an aggregate inside FILTER is 42803, matching PG (aggregates.md §11). A
    // non-boolean condition (or an untyped NULL, always unknown → folds no row) is 42804. The
    // fold loop evaluates this per row and folds only the rows for which it is TRUE.
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        "argument of FILTER must be type boolean",
                    ));
                }
            }
        }
        None => None,
    };
    // Aggregate results follow the group-key values in the synthetic row. A grouped+window query
    // (`GroupedWindow`) collects into the SAME `agg_specs` (its window results are slotted after
    // every aggregate — spec/design/window.md §5.1).
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand,
                distinct,
                filter: rfilter,
                osa: None,
                hypo: None,
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand,
                distinct,
                filter: rfilter,
                osa: None,
                hypo: None,
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("an aggregate in a non-collecting context is handled above"),
    }
}

/// Resolve an ordered-set aggregate `agg(direct_args) WITHIN GROUP (ORDER BY key)` — `mode`,
/// `percentile_cont`, `percentile_disc` (spec/design/aggregates.md §13). Like `resolve_aggregate`
/// it is valid only in a collecting context (else 42803) and folds into the same `AggSpec` list,
/// returning a synthetic-row reference. The `WITHIN GROUP` key is the aggregate's operand
/// (resolved with aggregates forbidden — a nested aggregate is 42803); the parenthesized `args`
/// are the per-group direct argument (the percentile fraction; empty for `mode`).
#[allow(clippy::too_many_arguments)]
fn resolve_ordered_set_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    keys: &[OrderKey],
    distinct: bool,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    // DISTINCT cannot decorate an ordered-set aggregate (PG: a 42601 syntax error).
    if distinct {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "DISTINCT is not allowed with ordered-set aggregates",
        ));
    }
    // Exactly one WITHIN GROUP sort key (PG models a second as a missing overload → 42883).
    let [key] = keys else {
        return Err(no_agg_overload(name));
    };
    // The aggregated argument: the WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN
    // (a nested aggregate in the order key is 42803, matching PG). A general-expression key
    // (`ORDER BY a + b`) carries a resolved `expr`; a bare/qualified column key carries `column`
    // (rebuilt here as an `Expr` so both paths share one resolve).
    let mut sub = AggCtx::Forbidden;
    let (operand, optype) = match &key.expr {
        Some(e) => resolve(scope, e, None, &mut sub, params)?,
        None => {
            let key_expr = match &key.qualifier {
                Some(q) => Expr::QualifiedColumn {
                    qualifier: q.clone(),
                    name: key.column.clone(),
                },
                None => Expr::Column(key.column.clone()),
            };
            resolve(scope, &key_expr, None, &mut sub, params)?
        }
    };
    // The WITHIN GROUP key's COLLATION drives the sort (aggregates.md §13): an explicit `COLLATE`
    // on the key (text operand only — else "collations are not supported by type T", like the query
    // ORDER BY), else a bare/qualified column key inherits its column's frozen collation; otherwise
    // the default `C` (byte) order. Resolved to the loaded `Collation` (42704 if not loaded). The
    // finalize sort applies it (an unmapped code point → 0A000 there).
    let collation: Option<std::sync::Arc<Collation>> = match &key.collation {
        Some(name) => {
            if !matches!(optype, ResolvedType::Text) {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    optype.type_name()
                )));
            }
            resolve_collation_name(scope.catalog, name)?
        }
        None => match (&key.expr, &key.qualifier, &key.column) {
            // A bare/qualified column key with no explicit COLLATE inherits the column's collation.
            (None, q, col) => {
                let r = match q {
                    Some(q) => scope.resolve_qualified(q, col)?,
                    None => scope.resolve_bare(col)?,
                };
                match &scope.column_of(r).collation {
                    Some(cn) => resolve_collation_name(scope.catalog, cn)?,
                    None => None,
                }
            }
            _ => None,
        },
    };

    let lname = name.to_ascii_lowercase();
    let (plan, frac, result) = match lname.as_str() {
        "mode" => {
            // mode() takes no direct argument; mode(x) matches no overload (42883).
            if !args.is_empty() {
                return Err(no_agg_overload(&lname));
            }
            (AggPlan::OrderedSetMode, None, optype.clone())
        }
        "percentile_disc" => {
            // An ARRAY fraction (`percentile_disc(ARRAY[…])`) returns an array of percentiles, one
            // per element; a scalar fraction returns one value (aggregates.md §18).
            let (frac, is_array) = resolve_osa_fraction(scope, &lname, args, agg, params)?;
            let result = array_if(optype.clone(), is_array);
            (AggPlan::OrderedSetDisc, Some(frac), result)
        }
        "percentile_cont" => {
            // percentile_cont interpolates: over a NUMERIC input it widens to f64 and returns f64;
            // over an INTERVAL input it interpolates in the interval domain (PG `interval_lerp`) and
            // returns interval. Any other WITHIN GROUP type matches no overload (42883). An ARRAY
            // fraction makes the result an array of those percentiles (aggregates.md §18).
            let (frac, is_array) = resolve_osa_fraction(scope, &lname, args, agg, params)?;
            match optype {
                ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Float(_) => (
                    AggPlan::OrderedSetCont,
                    Some(frac),
                    array_if(ResolvedType::Float(ScalarType::Float64), is_array),
                ),
                ResolvedType::Interval => (
                    AggPlan::OrderedSetContInterval,
                    Some(frac),
                    array_if(ResolvedType::Interval, is_array),
                ),
                _ => return Err(no_agg_overload(&lname)),
            }
        }
        _ => unreachable!("is_ordered_set_aggregate_name gates the three names above"),
    };

    // FILTER (WHERE cond): resolved per input row with aggregates forbidden, exactly as for an
    // ordinary aggregate (aggregates.md §11) — a non-boolean cond is 42804, a nested aggregate
    // 42803. Composes with the ordered-set fold (the filter restricts the collected rows first).
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        "argument of FILTER must be type boolean",
                    ));
                }
            }
        }
        None => None,
    };

    let osa = OsaParams {
        desc: key.descending,
        frac,
        collation,
    };
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand: Some(operand),
                distinct: false,
                filter: rfilter,
                osa: Some(osa),
                hypo: None,
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand: Some(operand),
                distinct: false,
                filter: rfilter,
                osa: Some(osa),
                hypo: None,
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("the non-collecting context is rejected above"),
    }
}

/// Whether `name` is a hypothetical-set aggregate surface — `rank`/`dense_rank`/`percent_rank`/
/// `cume_dist` used with `WITHIN GROUP` (spec/design/aggregates.md §19). These names are *also*
/// window functions; the `WITHIN GROUP` clause routes them here instead of the window path.
fn is_hypothetical_set_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "rank" | "dense_rank" | "percent_rank" | "cume_dist"
    )
}

/// Resolve a hypothetical-set aggregate `f(direct_args) WITHIN GROUP (ORDER BY keys)` — `rank`,
/// `dense_rank`, `percent_rank`, `cume_dist` (spec/design/aggregates.md §19). The direct args are the
/// hypothetical row; the `WITHIN GROUP` keys are the sort columns. Their counts must match (else
/// `42883`). Each key operand is buffered per row; each direct arg is evaluated per group (it may
/// reference grouping columns) and coerced to the key's type. Like the other ordered-set aggregates,
/// `OVER` is `0A000`, `DISTINCT` is `42601`, and it is valid only in a collecting context.
#[allow(clippy::too_many_arguments)]
fn resolve_hypothetical_set_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    keys: &[OrderKey],
    distinct: bool,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    if distinct {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "DISTINCT is not allowed with ordered-set aggregates",
        ));
    }
    let lname = name.to_ascii_lowercase();
    // The number of hypothetical direct arguments must match the number of ordering columns (PG
    // models a mismatch as a missing overload → 42883).
    if args.is_empty() || args.len() != keys.len() {
        return Err(no_agg_overload(&lname));
    }
    // Resolve each WITHIN GROUP key operand (per row, aggregates forbidden) + its sort spec, then the
    // matching direct argument (per group, in the grouped context so it may reference grouping
    // columns) coerced to the key's type.
    let mut key_nodes: Vec<RExpr> = Vec::with_capacity(keys.len());
    let mut sorts: Vec<KeySort> = Vec::with_capacity(keys.len());
    let mut arg_nodes: Vec<RExpr> = Vec::with_capacity(args.len());
    for (key, arg) in keys.iter().zip(args.iter()) {
        // A nested aggregate in the key is 42803.
        let mut sub = AggCtx::Forbidden;
        let (knode, ktype) = match &key.expr {
            Some(e) => resolve(scope, e, None, &mut sub, params)?,
            None => {
                let key_expr = match &key.qualifier {
                    Some(q) => Expr::QualifiedColumn {
                        qualifier: q.clone(),
                        name: key.column.clone(),
                    },
                    None => Expr::Column(key.column.clone()),
                };
                resolve(scope, &key_expr, None, &mut sub, params)?
            }
        };
        // The key's collation (explicit COLLATE — text only — or a column's frozen collation), §13.
        let collation: Option<std::sync::Arc<Collation>> = match &key.collation {
            Some(cn) => {
                if !matches!(ktype, ResolvedType::Text) {
                    return Err(type_error(format!(
                        "collations are not supported by type {}",
                        ktype.type_name()
                    )));
                }
                resolve_collation_name(scope.catalog, cn)?
            }
            None => match (&key.expr, &key.qualifier, &key.column) {
                (None, q, col) => {
                    let r = match q {
                        Some(q) => scope.resolve_qualified(q, col)?,
                        None => scope.resolve_bare(col)?,
                    };
                    match &scope.column_of(r).collation {
                        Some(cn) => resolve_collation_name(scope.catalog, cn)?,
                        None => None,
                    }
                }
                _ => None,
            },
        };
        // The hypothetical direct arg, evaluated per group (grouped context); a literal adapts to the
        // key's scalar type via the hint. Its type must match the key's family (else 42883).
        let hint = match type_from_resolved(&ktype) {
            Ok(Type::Scalar(s)) => Some(s),
            _ => None,
        };
        let (anode, atype) = resolve(scope, arg, hint, agg, params)?;
        if !hypo_arg_compatible(&atype, &ktype) {
            return Err(no_agg_overload(&lname));
        }
        key_nodes.push(knode);
        sorts.push(KeySort {
            desc: key.descending,
            nulls_first: key.nulls_first,
            collation,
        });
        arg_nodes.push(anode);
    }

    // FILTER (WHERE cond): per-input-row predicate (aggregates forbidden); restricts buffered rows.
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => return Err(type_error("argument of FILTER must be type boolean")),
            }
        }
        None => None,
    };

    let (plan, result) = match lname.as_str() {
        "rank" => (AggPlan::HypoRank, ResolvedType::Int(ScalarType::Int64)),
        "dense_rank" => (AggPlan::HypoDenseRank, ResolvedType::Int(ScalarType::Int64)),
        "percent_rank" => (
            AggPlan::HypoPercentRank,
            ResolvedType::Float(ScalarType::Float64),
        ),
        "cume_dist" => (
            AggPlan::HypoCumeDist,
            ResolvedType::Float(ScalarType::Float64),
        ),
        _ => unreachable!("is_hypothetical_set_name gates the four names above"),
    };
    let hypo = HypoParams {
        args: arg_nodes,
        keys: key_nodes,
        sorts,
    };
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand: None,
                distinct: false,
                filter: rfilter,
                osa: None,
                hypo: Some(hypo),
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand: None,
                distinct: false,
                filter: rfilter,
                osa: None,
                hypo: Some(hypo),
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("the non-collecting context is rejected above"),
    }
}

/// Whether a hypothetical direct argument of type `arg` is comparable with the `WITHIN GROUP` key of
/// type `key` (aggregates.md §19). A `NULL` arg is always allowed; otherwise the two must be the same
/// scalar family (numeric `Int`/`Decimal`/`Float` interconvert, since the value comparator orders
/// them by value), so the buffered key tuple and the hypothetical row compare meaningfully.
fn hypo_arg_compatible(arg: &ResolvedType, key: &ResolvedType) -> bool {
    use ResolvedType::*;
    if matches!(arg, Null) {
        return true;
    }
    matches!(
        (arg, key),
        (Int(_), Int(_))
            | (Decimal, Decimal)
            | (Float(_), Float(_))
            | (Text, Text)
            | (Bool, Bool)
            | (Bytea, Bytea)
            | (Uuid, Uuid)
            | (Timestamp, Timestamp)
            | (Timestamptz, Timestamptz)
            | (Date, Date)
            | (Interval, Interval)
    )
}

/// Resolve an ordered-set aggregate's direct argument — the percentile fraction (aggregates.md
/// §13/§17). The fraction is evaluated **once per group**, so it may be any expression over
/// **grouping columns** (resolved here in the grouped `agg` context, so a grouping column binds its
/// synthetic key slot and a non-grouped column is `42803` — PG's *"direct arguments … must use only
/// grouped columns"*); a constant folds the usual way. An aggregate inside the fraction is `42803`
/// (PG forbids nesting). Resolved with a float hint so a bare numeric literal folds to `f64`. The
/// returned node is stored and evaluated per group at finalize. Returns `(node, is_array)` — a
/// NUMERIC array fraction (`percentile_cont(ARRAY[…])`) computes one percentile per element and
/// returns an array (§18). A non-numeric fraction or a wrong argument count matches no overload
/// (`42883`); a NULL fraction yields a NULL result at finalize.
fn resolve_osa_fraction(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, bool)> {
    let [arg] = args else {
        return Err(no_agg_overload(name)); // wrong argument count
    };
    // The fraction is evaluated before the fold (it is a direct argument, not an aggregate operand),
    // so a nested aggregate is illegal — 42803, matching PG.
    if expr_has_aggregate(arg) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate function calls cannot be nested",
        ));
    }
    let (rarg, rtype) = resolve(scope, arg, Some(ScalarType::Float64), agg, params)?;
    match rtype {
        ResolvedType::Null
        | ResolvedType::Float(_)
        | ResolvedType::Int(_)
        | ResolvedType::Decimal => Ok((rarg, false)),
        ResolvedType::Array(elem)
            if matches!(
                *elem,
                ResolvedType::Float(_) | ResolvedType::Int(_) | ResolvedType::Decimal
            ) =>
        {
            Ok((rarg, true))
        }
        _ => Err(no_agg_overload(name)), // a non-numeric fraction matches no overload
    }
}

/// `Array(t)` when `is_array`, else `t` — the result type of an ordered-set aggregate whose direct
/// argument is an array vs. a scalar fraction (aggregates.md §18).
fn array_if(t: ResolvedType, is_array: bool) -> ResolvedType {
    if is_array {
        ResolvedType::Array(Box::new(t))
    } else {
        t
    }
}

/// Resolve `GROUPING(c1, …, ck)` (spec/design/aggregates.md §12) — the grouping-sets membership
/// function. Valid only in a grouped query's projection / HAVING (`Collect`); each argument must be
/// one of the master grouping columns, else 42803 (matching PostgreSQL). Returns an `integer` (i32)
/// whose bit `(k-1-j)` is 1 iff `c_j` is grouped away in the row's grouping set. The value is computed
/// per group row at execution from the grouping set's mask, so the call resolves to the placeholder
/// slot `GROUPING_GS_BASE + index` (rebased to its real trailing synthetic slot after resolution).
fn resolve_grouping(
    scope: &Scope,
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    _params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        // GROUPING(*) — PG raises a syntax error; mirror the COUNT-only `*` message (42601).
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    if args.is_empty() {
        // GROUPING() with no arguments — PG raises a syntax error (42601).
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "GROUPING requires at least one argument",
        ));
    }
    let grouping_arg_err = || {
        EngineError::new(
            SqlState::GroupingError,
            "arguments to GROUPING must be grouping expressions of the associated query level",
        )
    };
    // GROUPING is meaningful only in a grouped query — `Collect`, or `GroupedWindow` when the query
    // also has window functions (GROUPING SETS + window, aggregates.md §21). Outside one (Forbidden /
    // Window) its arguments cannot be grouping expressions.
    let group_keys: Vec<usize> = match agg {
        AggCtx::Collect { group_keys, .. } | AggCtx::GroupedWindow { group_keys, .. } => {
            group_keys.clone()
        }
        _ => return Err(grouping_arg_err()),
    };
    // Map each argument (a grouping column) to its master-grouping-column position.
    let mut positions: Vec<usize> = Vec::with_capacity(args.len());
    for arg in args {
        let r = match arg {
            Expr::Column(name) => scope.resolve_bare(name)?,
            Expr::QualifiedColumn { qualifier, name } => {
                scope.resolve_qualified(qualifier, name)?
            }
            // A non-column argument is never a grouping column (jed groups by columns only).
            _ => return Err(grouping_arg_err()),
        };
        let idx = match r {
            Resolved::Local(idx) => idx,
            Resolved::Outer { .. } => return Err(grouping_arg_err()),
        };
        match group_keys.iter().position(|&g| g == idx) {
            Some(p) => positions.push(p),
            None => return Err(grouping_arg_err()),
        }
    }
    let slot = match agg {
        AggCtx::Collect { grouping_specs, .. } | AggCtx::GroupedWindow { grouping_specs, .. } => {
            let s = GROUPING_GS_BASE + grouping_specs.len();
            grouping_specs.push(positions);
            s
        }
        _ => unreachable!("Collect / GroupedWindow verified above"),
    };
    Ok((RExpr::Column(slot), ResolvedType::Int(ScalarType::Int32)))
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
    let ty = resolved_type_of_col(&scope.column_at(idx).ty, scope.catalog);
    match agg {
        // Forbidden and Window both read the real input row by flat index (a plain window query's
        // bare columns are not grouped — spec/design/window.md §5.1).
        AggCtx::Forbidden | AggCtx::Window { .. } => Ok((RExpr::Column(idx), ty)),
        // Collect and GroupedWindow require a grouping key, resolved to its synthetic group-key slot.
        AggCtx::Collect { group_keys, .. } | AggCtx::GroupedWindow { group_keys, .. } => {
            match group_keys.iter().position(|&gk| gk == idx) {
                Some(pos) => Ok((RExpr::Column(pos), ty)),
                None => Err(grouping_error_column(name)),
            }
        }
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

/// Whether `name` (case-insensitive) is a registered aggregate surface (COUNT/SUM/MIN/MAX/AVG).
/// Data-driven over the catalog (`AGGREGATES`); consulted by the grouping + CHECK-structure walks.
/// The ordered-set aggregates (`is_ordered_set_aggregate_name`) are aggregates for these purposes
/// too — they fold a set of rows — but are not catalog rows (their result/arg mold is special,
/// like `GROUPING()`), so they are OR'd in here rather than carried in `AGGREGATES`.
fn is_aggregate_name(name: &str) -> bool {
    AGGREGATES
        .iter()
        .any(|a| a.surface.eq_ignore_ascii_case(name))
        || object_agg_classify(name).is_some()
        || is_ordered_set_aggregate_name(name)
}

/// Classify a `json[b]_object_agg[_unique]` name → (is-json, is-unique). `None` otherwise. These
/// 2-argument aggregates are hand-resolved (the single-operand aggregate catalog can't express a
/// key/value pair), like `jsonb_set` among the scalar functions. (json-sql-functions.md §4)
fn object_agg_classify(name: &str) -> Option<(bool, bool)> {
    match name.to_ascii_lowercase().as_str() {
        "jsonb_object_agg" => Some((false, false)),
        "json_object_agg" => Some((true, false)),
        "jsonb_object_agg_unique" => Some((false, true)),
        "json_object_agg_unique" => Some((true, true)),
        _ => None,
    }
}

/// Whether `name` is an ordered-set aggregate surface (`mode`/`percentile_cont`/`percentile_disc` —
/// spec/design/aggregates.md §13). These take a `WITHIN GROUP (ORDER BY …)` clause and are resolved
/// by `resolve_ordered_set_aggregate`, intercepted before the generic aggregate/scalar dispatch.
fn is_ordered_set_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "mode" | "percentile_cont" | "percentile_disc"
    )
}

/// Whether `name` is a registered WINDOW-only function surface (row_number/rank/…). Data-driven
/// over the catalog (`WINDOWS`). Such a function REQUIRES an `OVER` clause — used without one it is
/// 42P20 (spec/design/window.md §7). The catalog aggregates double as window functions but are not
/// in `WINDOWS`, so they are still valid without `OVER`.
fn is_window_only_name(name: &str) -> bool {
    WINDOWS.iter().any(|w| w.surface.eq_ignore_ascii_case(name))
}

/// Resolve a window-function call `f(args) OVER (window_definition)` (spec/design/window.md §5.1).
/// Valid only in a window query's projection (`AggCtx::Window`); anywhere else (WHERE / JOIN ON /
/// HAVING / an aggregate query) it is 42P20. The call collects into a `WindowSpec` and resolves to
/// the synthetic slot `base + window_index`. S0: only `row_number()`.
#[allow(clippy::too_many_arguments)]
fn resolve_window_call(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    star: bool,
    wd: &WindowDef,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_ascii_lowercase();
    // Validate the context and build the sub-context window ARGUMENTS *and keys* resolve in
    // (spec/design/window.md §5.1). In a grouped query (`GroupedWindow`) they resolve against the
    // grouped row — a nested aggregate collects into the query's SHARED `agg_specs` and a bare column
    // must be a grouping key (else 42803) — so we resolve them under a `Collect` borrowing those
    // specs; a nested window is then 42P20 (a `Collect` cannot collect a window). In a plain window
    // query they resolve under `Forbidden` (no aggregate/window nesting). A non-column PARTITION BY /
    // ORDER BY key is materialized into the query-global `window_keys` (taken out here, restored at the
    // end). The window result's slot + the spec push happen at the end, once the def is resolved.
    let (mut sub, mut window_keys): (AggCtx, Vec<RExpr>) = match agg {
        AggCtx::GroupedWindow {
            group_keys,
            group_key_exprs,
            agg_specs,
            grouping_specs,
            window_keys,
            ..
        } => (
            AggCtx::Collect {
                group_keys: group_keys.clone(),
                group_key_exprs: group_key_exprs.clone(),
                specs: std::mem::take(agg_specs),
                grouping_specs: std::mem::take(grouping_specs),
            },
            std::mem::take(window_keys),
        ),
        AggCtx::Window { window_keys, .. } => (AggCtx::Forbidden, std::mem::take(window_keys)),
        _ => {
            return Err(EngineError::new(
                SqlState::WindowingError,
                "window functions are not allowed here",
            ));
        }
    };
    // The plan + result type from the function name. S0: only row_number(); an aggregate name with
    // OVER (a window aggregate) is deferred to S3; any other name is 42883.
    // The frame-insensitive no-argument ranking functions (S0/S1): row_number/rank/dense_rank → i64.
    let no_arg_i64 = match lname.as_str() {
        "row_number" => Some(WindowPlan::RowNumber),
        "rank" => Some(WindowPlan::Rank),
        "dense_rank" => Some(WindowPlan::DenseRank),
        _ => None,
    };
    // The frame-insensitive no-argument ratio functions (S1): percent_rank/cume_dist → f64
    // (PG's float8 — the ratio is the IEEE correctly-rounded f64 division, window.md §4).
    let no_arg_ratio = match lname.as_str() {
        "percent_rank" => Some(WindowPlan::PercentRank),
        "cume_dist" => Some(WindowPlan::CumeDist),
        _ => None,
    };
    let mut wargs: Vec<RExpr> = Vec::new();
    let (plan, result) = if let Some(p) = no_arg_i64 {
        if star || !args.is_empty() {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes no arguments"),
            ));
        }
        (p, ResolvedType::Int(ScalarType::Int64))
    } else if let Some(p) = no_arg_ratio {
        if star || !args.is_empty() {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes no arguments"),
            ));
        }
        (p, ResolvedType::Float(ScalarType::Float64))
    } else if lname == "ntile" {
        // ntile(n) — one integer bucket-count argument (window.md §4), resolved in a fresh
        // Forbidden sub-context (no aggregate/window nesting in a window argument).
        if star || args.len() != 1 {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                "ntile takes exactly one argument",
            ));
        }
        let (anode, aty) = resolve(scope, &args[0], None, &mut sub, params)?;
        if !matches!(aty, ResolvedType::Int(_) | ResolvedType::Null) {
            return Err(type_error("argument of ntile must be integer"));
        }
        wargs.push(anode);
        (WindowPlan::Ntile, ResolvedType::Int(ScalarType::Int64))
    } else if lname == "lag" || lname == "lead" {
        // lag/lead(value [, offset [, default]]) — window.md §4. The value expression's type is the
        // result; offset is an integer (default 1); default (returned when the offset leaves the
        // partition) must match the value type. Args resolved in a fresh Forbidden sub-context.
        if star || args.is_empty() || args.len() > 3 {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes 1 to 3 arguments"),
            ));
        }
        let (vnode, vty) = resolve(scope, &args[0], None, &mut sub, params)?;
        let hint = match &vty {
            ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
            _ => None,
        };
        wargs.push(vnode);
        if args.len() >= 2 {
            let (onode, oty) = resolve(scope, &args[1], None, &mut sub, params)?;
            if !matches!(oty, ResolvedType::Int(_) | ResolvedType::Null) {
                return Err(type_error(format!("offset of {lname} must be integer")));
            }
            wargs.push(onode);
        }
        if args.len() == 3 {
            let (dnode, dty) = resolve(scope, &args[2], hint, &mut sub, params)?;
            if dty != ResolvedType::Null && dty != vty {
                return Err(type_error(format!(
                    "default of {lname} must match the value type"
                )));
            }
            wargs.push(dnode);
        }
        let plan = if lname == "lag" {
            WindowPlan::Lag
        } else {
            WindowPlan::Lead
        };
        (plan, vty)
    } else if is_aggregate_name(&lname) {
        // An aggregate used as a window function (S3): reuse the aggregate overload resolution to
        // get the plan + result type; apply_window_stage folds it over the default frame (running
        // with a window ORDER BY, whole-partition without — spec/design/window.md §6).
        let (aggplan, result) = if star {
            if !aggregate_has_star(&lname) {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "* is only valid as the argument of COUNT",
                ));
            }
            (AggPlan::CountStar, ResolvedType::Int(ScalarType::Int64))
        } else {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            let desc =
                lookup_aggregate_overload(&lname, &t).ok_or_else(|| no_agg_overload(&lname))?;
            let (plan, result) = aggregate_plan(&lname, desc.result, &t);
            wargs.push(r); // the aggregate operand → args[0]
            (plan, result)
        };
        (WindowPlan::Agg(aggplan), result)
    } else if lname == "first_value" || lname == "last_value" || lname == "nth_value" {
        // Frame-sensitive value pickers (S4, window.md §4). first/last_value take one value
        // expression (→ result type); nth_value takes the value + an integer position.
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let want = if lname == "nth_value" { 2 } else { 1 };
        if args.len() != want {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes {want} argument(s)"),
            ));
        }
        let (vnode, vty) = resolve(scope, &args[0], None, &mut sub, params)?;
        wargs.push(vnode);
        let plan = if lname == "first_value" {
            WindowPlan::FirstValue
        } else if lname == "last_value" {
            WindowPlan::LastValue
        } else {
            let (nnode, nty) = resolve(scope, &args[1], None, &mut sub, params)?;
            if !matches!(nty, ResolvedType::Int(_) | ResolvedType::Null) {
                return Err(type_error("position of nth_value must be integer"));
            }
            wargs.push(nnode);
            WindowPlan::NthValue
        };
        (plan, vty)
    } else {
        return Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("{lname} is not a window function"),
        ));
    };
    // Resolve the window definition (PARTITION BY / ORDER BY expressions → slots, explicit frame).
    // Keys resolve in `sub` (the grouped Collect, so a bare grouping column → its grouped-row slot
    // and an aggregate → an agg slot, else 42803; or plain Forbidden, columns → real input slots); a
    // non-column key materializes into `window_keys` at a `WINDOW_KEY_BASE + k` placeholder. window.md
    // §5.1.
    let (partition, order, frame) =
        resolve_window_def(scope, wd, &mut sub, &mut window_keys, params)?;
    // FILTER (WHERE cond) on a window aggregate (aggregates.md §20): a per-frame-row boolean over the
    // INPUT row, resolved with aggregates forbidden (a nested aggregate is 42803, a non-boolean 42804)
    // — exactly the non-window FILTER rule (§11). The window stage folds only the frame rows it keeps.
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => return Err(type_error("argument of FILTER must be type boolean")),
            }
        }
        None => None,
    };
    let spec = WindowSpec {
        plan,
        partition,
        order,
        args: wargs,
        frame,
        filter: rfilter,
    };
    // Append the spec and resolve the result slot (the PLACEHOLDER `WINDOW_RESULT_BASE + w`, rebased to
    // its real slot after the row layout is final — window.md §5.1). Restore the borrowed `agg_specs`
    // (any aggregate nested in an argument or a window key was collected into `sub`) and the
    // materialized `window_keys`.
    match agg {
        AggCtx::GroupedWindow {
            agg_specs,
            grouping_specs,
            window_specs,
            window_keys: wk,
            ..
        } => {
            if let AggCtx::Collect {
                specs,
                grouping_specs: gs,
                ..
            } = sub
            {
                *agg_specs = specs;
                *grouping_specs = gs;
            }
            *wk = window_keys;
            let slot = WINDOW_RESULT_BASE + window_specs.len();
            window_specs.push(spec);
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::Window {
            specs,
            window_keys: wk,
            ..
        } => {
            *wk = window_keys;
            let slot = WINDOW_RESULT_BASE + specs.len();
            specs.push(spec);
            Ok((RExpr::Column(slot), result))
        }
        // The entry match already rejected every other context with 42P20.
        _ => unreachable!("resolve_window_call entry validated the context"),
    }
}

/// Resolve the PARTITION BY and within-partition ORDER BY (→ sort keys) of an `OVER (...)` clause.
/// Each key is a **general expression** (spec/design/window.md §5.1) resolved against `key_ctx`: a
/// plain window query passes `Forbidden` (columns → real input slots, an aggregate is 42803), a
/// grouped one passes a `Collect` borrowing the query's aggregate specs (a bare column → its
/// grouping-column slot or 42803, an aggregate `sum(x)` collects → its agg slot). A bare-column /
/// aggregate key (`RExpr::Column`) keeps its real slot; any compound key is materialized into
/// `window_keys` at a `WINDOW_KEY_BASE + k` placeholder, evaluated per row before the window stage.
/// A key referencing an enclosing-query column (a correlated window) is rejected (`0A000`); a window
/// function inside a key is rejected by the `Forbidden`/`Collect` sub-context (`42P20`).
fn resolve_window_def(
    scope: &Scope,
    wd: &WindowDef,
    key_ctx: &mut AggCtx,
    window_keys: &mut Vec<RExpr>,
    params: &mut ParamTypes,
) -> Result<(
    Vec<usize>,
    Vec<crate::spill::SortKey>,
    Option<ResolvedFrame>,
)> {
    let mut partition = Vec::with_capacity(wd.partition.len());
    for key in &wd.partition {
        let (rexpr, _ty) = resolve(scope, key, None, key_ctx, params)?;
        partition.push(window_key_slot(rexpr, "PARTITION BY", window_keys)?);
    }
    let mut order: Vec<crate::spill::SortKey> = Vec::with_capacity(wd.order.len());
    // The ORDER BY key types, captured in lockstep with `order` — a `RANGE` value-offset frame folds
    // `key ± offset` over the single ordering key, so it needs the key's type (§6).
    let mut order_types: Vec<Type> = Vec::with_capacity(wd.order.len());
    for key in &wd.order {
        let (rexpr, ty) = resolve(scope, &key.expr, None, key_ctx, params)?;
        // The sort-key collation. An explicit trailing `COLLATE` (rare — `parse_expr` usually absorbs
        // a `COLLATE` into the key expression) must be on a text key (42804); otherwise the collation
        // is DERIVED from the key expression (collation.md §1) — a `COLLATE` inside it is explicit, a
        // bare text column is its frozen implicit collation, every other shape resets to none (C). A
        // collated window ORDER BY honors the collation in both the per-partition sort and peer
        // determination (window.md §3/§5); `COLLATE "C"` resolves to `None` (the raw-byte fast path).
        let coll = match &key.collation {
            Some(cn) => {
                if !matches!(ty, ResolvedType::Text | ResolvedType::Null) {
                    return Err(type_error(format!(
                        "collations are not supported by type {}",
                        ty.type_name()
                    )));
                }
                resolve_collation_name(scope.catalog, cn)?
            }
            None => resolve_deriv(scope.catalog, derive_collation(scope, &key.expr)?)?,
        };
        let slot = window_key_slot(rexpr, "window ORDER BY", window_keys)?;
        order.push((slot, key.descending, key.nulls_first, coll));
        order_types.push(type_from_resolved(&ty)?);
    }
    // The explicit frame (window.md §6): ROWS / GROUPS integer-count offsets, RANGE value offsets.
    let frame = match &wd.frame {
        None => None,
        Some(f) => Some(resolve_frame(f, &order, &order_types)?),
    };
    Ok((partition, order, frame))
}

/// Map a resolved window-key expression to the slot the window stage indexes (spec/design/window.md
/// §5.1). A bare column / aggregate (`RExpr::Column`) keeps its real row slot — the input slot for a
/// plain query, the grouped-row slot for a grouped one — so a column-only window is byte-identical to
/// before. Any compound expression is materialized into `window_keys` at the placeholder slot
/// `WINDOW_KEY_BASE + k` (rebased once the row layout is final). A key referencing an enclosing query
/// (a correlated window — `where` names the clause) is the deferred follow-on (`0A000`).
fn window_key_slot(rexpr: RExpr, clause: &str, window_keys: &mut Vec<RExpr>) -> Result<usize> {
    if rexpr_references_outer(&rexpr, 0) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("{clause} may not reference an outer query column"),
        ));
    }
    Ok(match rexpr {
        RExpr::Column(i) => i,
        other => {
            let k = window_keys.len();
            window_keys.push(other);
            WINDOW_KEY_BASE + k
        }
    })
}

/// Resolve an explicit frame clause (spec/design/window.md §6). `GROUPS` requires an ORDER BY
/// (`42P20`); a `RANGE` value offset requires exactly one ORDER BY column (`42P20`) of an integer,
/// decimal, or float type (a timestamp/date key is the deferred D4 follow-on, any other type is
/// `0A000`). A negative offset is `22013`; `EXCLUDE` was already rejected at parse.
fn resolve_frame(
    f: &crate::ast::WindowFrame,
    order: &[crate::spill::SortKey],
    order_types: &[Type],
) -> Result<ResolvedFrame> {
    use crate::ast::{FrameBound, FrameMode};
    let is_offset =
        |b: &FrameBound| matches!(b, FrameBound::Preceding(_) | FrameBound::Following(_));
    let has_offset = is_offset(&f.start) || is_offset(&f.end);
    match f.mode {
        FrameMode::Rows => Ok(ResolvedFrame {
            mode: FrameMode::Rows,
            start: resolve_int_bound(&f.start)?,
            end: resolve_int_bound(&f.end)?,
            exclude: f.exclude,
        }),
        FrameMode::Groups => {
            if order.is_empty() {
                return Err(EngineError::new(
                    SqlState::WindowingError,
                    "GROUPS mode requires an ORDER BY clause",
                ));
            }
            Ok(ResolvedFrame {
                mode: FrameMode::Groups,
                start: resolve_int_bound(&f.start)?,
                end: resolve_int_bound(&f.end)?,
                exclude: f.exclude,
            })
        }
        FrameMode::Range if has_offset => {
            if order.len() != 1 {
                return Err(EngineError::new(
                    SqlState::WindowingError,
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
                ));
            }
            let kt = &order_types[0];
            if !(kt.is_integer() || kt.is_decimal() || kt.is_float()) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {}",
                        kt.canonical_name()
                    ),
                ));
            }
            Ok(ResolvedFrame {
                mode: FrameMode::Range,
                start: resolve_range_bound(&f.start, kt)?,
                end: resolve_range_bound(&f.end, kt)?,
                exclude: f.exclude,
            })
        }
        // RANGE with only UNBOUNDED / CURRENT ROW bounds — peer/edge based, any number of ORDER BY
        // keys (or none); no key arithmetic, so it reuses the plain bound resolution.
        FrameMode::Range => Ok(ResolvedFrame {
            mode: FrameMode::Range,
            start: resolve_int_bound(&f.start)?,
            end: resolve_int_bound(&f.end)?,
            exclude: f.exclude,
        }),
    }
}

/// Resolve a ROWS/GROUPS frame bound: the offset of `n PRECEDING`/`n FOLLOWING` must be a
/// non-negative integer literal (`22013` if negative; a non-literal/non-integer offset is `0A000`).
fn resolve_int_bound(b: &crate::ast::FrameBound) -> Result<ResolvedBound> {
    use crate::ast::FrameBound;
    let offset = |e: &Expr| -> Result<Value> {
        match e {
            Expr::Literal(Literal::Int(n)) if *n >= 0 => Ok(Value::Int(*n)),
            Expr::Literal(Literal::Int(_)) => Err(EngineError::new(
                SqlState::InvalidPrecedingOrFollowingSize,
                "frame starting or ending offset must not be negative",
            )),
            _ => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "frame offset must be a non-negative integer literal",
            )),
        }
    };
    Ok(match b {
        FrameBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        FrameBound::CurrentRow => ResolvedBound::CurrentRow,
        FrameBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
        FrameBound::Preceding(e) => ResolvedBound::Preceding(offset(e)?),
        FrameBound::Following(e) => ResolvedBound::Following(offset(e)?),
    })
}

/// Resolve a RANGE value-offset bound (window.md §6). The offset literal must be a non-negative
/// numeric matching the ordering key type: an integer key takes an integer offset (a decimal offset
/// is `0A000`, matching PG); a decimal key takes an integer (widened) or decimal offset; a **float**
/// key takes an integer or decimal offset converted to `f64` (PG's `in_range_float*_float8` — the
/// offset is `float8` for both `f32` and `f64` keys, window.md §6). The decimal→`f64` conversion
/// traps `22003` on overflow (jed's float-cast rule, a negligible micro-divergence from PG's
/// accept-infinite-offset); an int offset is always finite.
fn resolve_range_bound(b: &crate::ast::FrameBound, kt: &Type) -> Result<ResolvedBound> {
    use crate::ast::FrameBound;
    let neg = || {
        EngineError::new(
            SqlState::InvalidPrecedingOrFollowingSize,
            "frame starting or ending offset must not be negative",
        )
    };
    let offset = |e: &Expr| -> Result<Value> {
        match e {
            Expr::Literal(Literal::Int(n)) => {
                if *n < 0 {
                    return Err(neg());
                }
                if kt.is_float() {
                    Ok(Value::Float64(*n as f64))
                } else if kt.is_decimal() {
                    Ok(Value::Decimal(Decimal::from_i64(*n)))
                } else {
                    Ok(Value::Int(*n))
                }
            }
            Expr::Literal(Literal::Decimal(d)) => {
                if d.is_negative() {
                    return Err(neg());
                }
                if kt.is_float() {
                    decimal_to_float(d, ScalarType::Float64)
                } else if kt.is_decimal() {
                    Ok(Value::Decimal(d.clone()))
                } else {
                    Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {} and offset type decimal",
                            kt.canonical_name()
                        ),
                    ))
                }
            }
            _ => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "frame offset must be a non-negative numeric literal",
            )),
        }
    };
    Ok(match b {
        FrameBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        FrameBound::CurrentRow => ResolvedBound::CurrentRow,
        FrameBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
        FrameBound::Preceding(e) => ResolvedBound::Preceding(offset(e)?),
        FrameBound::Following(e) => ResolvedBound::Following(offset(e)?),
    })
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
/// (abs/round/…, spec/design/functions.md §9), the named/defaulted `make_interval` (§11), or
/// 42883 (undefined_function) for any other name. Aggregates and scalar functions share the call
/// syntax (grammar.md §17); they are distinguished here, at resolve. Named notation (`name =>
/// value`) is valid only for a function that declares parameter names (make_interval); on every
/// other function it is rejected 42883 (PG's "function ... has no parameter named X").
fn resolve_func_call(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
    star: bool,
    distinct: bool,
    filter: Option<&Expr>,
    variadic: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_ascii_lowercase();
    // DISTINCT is an aggregate-only modifier: `abs(DISTINCT x)` is 42809 (PG's wrong_object_type,
    // "DISTINCT specified, but <fn> is not an aggregate function" — aggregates.md §5). Checked
    // before the per-kind dispatch so it covers every non-aggregate path (scalar, array, …).
    if distinct && !is_aggregate_name(&lname) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!("DISTINCT specified, but {lname} is not an aggregate function"),
        ));
    }
    // FILTER is likewise aggregate-only: `abs(x) FILTER (WHERE …)` is 42809 (PG's wrong_object_type,
    // "FILTER specified, but <fn> is not an aggregate function" — aggregates.md §11). Same placement
    // as DISTINCT, so it covers every non-aggregate path before the per-kind dispatch.
    if filter.is_some() && !is_aggregate_name(&lname) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!("FILTER specified, but {lname} is not an aggregate function"),
        ));
    }
    // The VARIADIC keyword is only valid on a VARIADIC function (array-functions.md §12). It
    // cannot decorate make_interval / an aggregate / an ordinary scalar function (PG: "VARIADIC
    // argument must be an array" arises only on a variadic function; a non-variadic function with
    // VARIADIC is 42883 — no such overload). Caught here before the per-kind dispatch.
    if variadic && !is_variadic_func_name(&lname) {
        return Err(no_func_overload(&lname));
    }
    if is_variadic_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_variadic_func(scope, &lname, args, star, variadic, agg, params);
    }
    // make_interval is the one named/defaulted function — it keeps its own resolver (§11).
    if lname == "make_interval" {
        return resolve_make_interval(scope, args, arg_names, star, agg, params);
    }
    // make_timestamp / make_timestamptz are its named (un-defaulted) siblings (§11); make_timestamptz
    // is overloaded on arity (a session-zone 6-arg form + an explicit-zone 7-arg form). Their own
    // resolver picks the overload and normalizes named notation.
    if lname == "make_timestamp" || lname == "make_timestamptz" {
        return resolve_make_timestamp(scope, &lname, args, arg_names, star, agg, params);
    }
    // lower/upper are overloaded across TWO families: the range accessors (range → element,
    // range-functions.md §1) and the text casing functions (text → text, collation.md §16). Resolve
    // the single argument once and branch on its type, BEFORE the by-name kind dispatch (which would
    // force the range path for both). (functions.md §9)
    if lname == "lower" || lname == "upper" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_lower_upper(scope, &lname, args, agg, params);
    }
    // `timezone(zone, value)` is the desugar of `value AT TIME ZONE zone` (grammar.md §49,
    // timezones.md §6) and a callable function in its own right. It is overloaded on the value's
    // family (timestamptz → timestamp, timestamp → timestamptz), so it resolves before the generic
    // by-name dispatch (which has no such polymorphism). (functions.md §9)
    if lname == "timezone" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_timezone(scope, args, agg, params);
    }
    // `date_trunc(unit, value[, zone])` (timezones.md §9.1) — polymorphic on the value family (the
    // result type is the value type) + an optional 3rd zone arg only on a timestamptz, so it resolves
    // before the generic by-name dispatch (which has no such polymorphism).
    if lname == "date_trunc" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_date_trunc(scope, args, agg, params);
    }
    // GROUPING(c1, …, ck) — the grouping-sets membership function (spec/design/aggregates.md §12).
    // It is not an aggregate (no DISTINCT/FILTER — those already errored 42809 above) and only
    // resolves inside a grouped query, so it is intercepted before the by-name dispatch.
    if lname == "grouping" {
        reject_named(&lname, arg_names)?;
        return resolve_grouping(scope, args, star, agg, params);
    }
    // `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2) take a jsonb target, a text[] path (a
    // bare `'{a,b}'` literal adapts, like `#>`), a jsonb new value, and an optional boolean flag.
    // Hand-resolved (like the accessor operators) — the text[] + adapting-literal + optional-flag
    // signature is outside the catalog family mold.
    if lname == "jsonb_set" || lname == "jsonb_insert" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let mode = if lname == "jsonb_set" {
            json::PathSetMode::Set
        } else {
            json::PathSetMode::Insert
        };
        return resolve_jsonb_set_insert(scope, &lname, mode, args, agg, params);
    }
    // `json_object` / `jsonb_object` (json-sql-functions.md §2) build an object from one text[] of
    // alternating keys/values, or two text[] (keys, values). Hand-resolved (the text[] arg + adapting
    // literal are outside the catalog family mold), like jsonb_set.
    if lname == "json_object" || lname == "jsonb_object" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_json_object(scope, &lname, lname == "json_object", args, agg, params);
    }
    // The scalar jsonpath query functions (P2, jsonpath.md §5): `(ctx jsonb, path jsonpath)`. Hand-
    // resolved (the jsonpath arg + adapting-literal are outside the catalog family mold).
    if let Some(kind) = match lname.as_str() {
        "jsonb_path_exists" => Some(JsonPathFnKind::Exists),
        "jsonb_path_query_first" => Some(JsonPathFnKind::QueryFirst),
        "jsonb_path_query_array" => Some(JsonPathFnKind::QueryArray),
        "jsonb_path_match" => Some(JsonPathFnKind::Match),
        _ => None,
    } {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_jsonpath_fn(scope, &lname, kind, args, agg, params);
    }
    // Otherwise the registry (the catalog descriptor tables) decides whether the name is an
    // aggregate, a scalar function, or undefined — no hand-written name lists (extensibility.md §5).
    if is_aggregate_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_aggregate(scope, &lname, args, star, distinct, filter, agg, params);
    }
    // The polymorphic array functions (array-functions.md §2) are also kind="function", so they
    // must be intercepted BEFORE the generic scalar path — their `anyarray`/`anyelement` slots need
    // §2 unification, which `lookup_scalar_overload`'s exact-family match cannot do.
    if is_array_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_array_func(scope, &lname, args, agg, params);
    }
    if is_range_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_range_func(scope, &lname, args, agg, params);
    }
    // A range CONSTRUCTOR (range-functions.md §2): a call whose name is a range type name/alias.
    // Like the array/range functions it is kind="function", so it must be intercepted BEFORE the
    // generic scalar path (its concrete-range result + element coercion are not the family-matched
    // scalar mold).
    if is_range_ctor_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_range_ctor(scope, &lname, args, agg, params);
    }
    // The regex scalar functions (regex.md §8) are kind="function" too, but return text / text[] via
    // a dedicated RegexFunc node — the scalar-result path cannot carry the array result — so they are
    // intercepted before it, like the array/range functions above.
    if matches!(
        lname.as_str(),
        "regexp_replace"
            | "regexp_match"
            | "regexp_like"
            | "regexp_count"
            | "regexp_substr"
            | "regexp_instr"
    ) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_regex_func(scope, &lname, args, agg, params);
    }
    // `div(a, b)` — the truncated (toward zero) integer quotient of two numerics, at scale 0 (PG
    // div(numeric, numeric)). Resolver-routed because the catalog name "div" already belongs to the
    // `/` operator (verify.rb keys uniqueness on [name, arg_families], so a function row would clash
    // with the `/` decimal row). Accepts integer + decimal operands (integers promote to numeric, as
    // PG does); a float/other operand → 42883. Two-arg only; else fall through to 42883.
    if lname == "div" && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let (rl, lt, rr, rt) = resolve_operand_pair(scope, &args[0], &args[1], agg, params)?;
        let numeric_ok = |t: &ResolvedType| {
            matches!(
                t,
                ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
            )
        };
        if !numeric_ok(&lt) || !numeric_ok(&rt) {
            return Err(no_func_overload("div"));
        }
        return Ok((
            RExpr::ScalarFunc {
                func: ScalarFunc::Div,
                args: vec![rl, rr],
                result: ScalarType::Decimal,
            },
            resolved_type_of(ScalarType::Decimal),
        ));
    }
    // `gcd(a, b)` / `lcm(a, b)` — resolver-routed for the same integer-promotion the arithmetic
    // operators do (a function row's "promoted" result would take only the first operand's width).
    // EXACT/in-contract; integer → promoted integer, a decimal operand → numeric.
    if (lname == "gcd" || lname == "lcm") && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let (rl, rr, result) =
            resolve_int_or_decimal_pair(scope, &lname, &args[0], &args[1], agg, params)?;
        let func = if lname == "gcd" {
            ScalarFunc::Gcd
        } else {
            ScalarFunc::Lcm
        };
        return Ok((
            RExpr::ScalarFunc {
                func,
                args: vec![rl, rr],
                result,
            },
            resolved_type_of(result),
        ));
    }
    // `width_bucket(op, low, high, count)` — resolver-routed so the three value operands reconcile
    // across the integer/decimal families PG's implicit casts span (all-integer or mixed
    // integer/decimal → numeric; all-float → float). count must be integer. result int4.
    if lname == "width_bucket" && args.len() == 4 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let mut rargs = Vec::with_capacity(4);
        let mut tys = Vec::with_capacity(4);
        for a in args {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            rargs.push(r);
            tys.push(t);
        }
        // count (4th) is integer (a NULL adapts and propagates).
        if !matches!(tys[3], ResolvedType::Int(_) | ResolvedType::Null) {
            return Err(no_func_overload("width_bucket"));
        }
        // The value trio is EITHER all float (+NULL) → the float kernel, OR all integer/decimal
        // (+NULL) → the numeric kernel; a float mixed with a decimal/integer is 42883.
        let any_float = tys[..3].iter().any(|t| matches!(t, ResolvedType::Float(_)));
        let ok = |t: &ResolvedType| {
            if any_float {
                matches!(t, ResolvedType::Float(_) | ResolvedType::Null)
            } else {
                matches!(
                    t,
                    ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
                )
            }
        };
        if !tys[..3].iter().all(ok) {
            return Err(no_func_overload("width_bucket"));
        }
        return Ok((
            RExpr::ScalarFunc {
                func: ScalarFunc::WidthBucket,
                args: rargs,
                result: ScalarType::Int32,
            },
            resolved_type_of(ScalarType::Int32),
        ));
    }
    // `mod(a, b)` is the function spelling of the `%` (mod) operator (catalog name "mod") — route it
    // to the SAME arithmetic machinery so mod() and % are observably identical (promotion, the
    // integer/decimal/float kernels, 22012/22003). PG's mod() is integer/numeric only; jed
    // additionally accepts mod(float), the `%`-over-float extension (oracle_overrides.toml). Only the
    // two-arg form is mod(); any other arity falls through to 42883.
    if lname == "mod" && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_binary(scope, BinaryOp::Mod, &args[0], &args[1], agg, params);
    }
    if is_scalar_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_scalar_func(scope, &lname, args, star, agg, params);
    }
    Err(EngineError::new(
        SqlState::UndefinedFunction,
        format!("function does not exist: {name}"),
    ))
}

/// Resolve `regexp_replace`/`regexp_match` (regex.md §8) and the Oracle-compat `regexp_like`/
/// `regexp_count`/`regexp_substr`/`regexp_instr` (regex.md §8b) → a [`RExpr::RegexFunc`] whose result
/// type lives in the surrounding [`ResolvedType`]. All are STRICT (NULL arg propagates). The text
/// slots (source, pattern, flags) require text-or-null; the numeric slots (start/N/endoption/subexpr)
/// require integer-or-null (a non-integer is 42883, jed's strict-typing stance). A constant pattern
/// is precompiled once here (the precompilation contract, regex.md §5) — but only when the
/// case-insensitive `i` flag is statically known (the flags arg absent or a constant).
fn resolve_regex_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // (func, flags arg index, the integer-typed argument positions) per name + arity. Source(0) and
    // pattern(1) are always text; everything else is an integer except the flags slot (regex.md §8b).
    let (func, flags_idx, int_positions): (RegexFunc, Option<usize>, &[usize]) =
        match (name, args.len()) {
            ("regexp_replace", 3) => (RegexFunc::Replace, None, &[]),
            ("regexp_replace", 4) => (RegexFunc::Replace, Some(3), &[]),
            ("regexp_match", 2) => (RegexFunc::Match, None, &[]),
            ("regexp_match", 3) => (RegexFunc::Match, Some(2), &[]),
            ("regexp_like", 2) => (RegexFunc::Like, None, &[]),
            ("regexp_like", 3) => (RegexFunc::Like, Some(2), &[]),
            ("regexp_count", 2) => (RegexFunc::Count, None, &[]),
            ("regexp_count", 3) => (RegexFunc::Count, None, &[2]),
            ("regexp_count", 4) => (RegexFunc::Count, Some(3), &[2]),
            ("regexp_substr", 2) => (RegexFunc::Substr, None, &[]),
            ("regexp_substr", 3) => (RegexFunc::Substr, None, &[2]),
            ("regexp_substr", 4) => (RegexFunc::Substr, None, &[2, 3]),
            ("regexp_substr", 5) => (RegexFunc::Substr, Some(4), &[2, 3]),
            ("regexp_substr", 6) => (RegexFunc::Substr, Some(4), &[2, 3, 5]),
            ("regexp_instr", 2) => (RegexFunc::Instr, None, &[]),
            ("regexp_instr", 3) => (RegexFunc::Instr, None, &[2]),
            ("regexp_instr", 4) => (RegexFunc::Instr, None, &[2, 3]),
            ("regexp_instr", 5) => (RegexFunc::Instr, None, &[2, 3, 4]),
            ("regexp_instr", 6) => (RegexFunc::Instr, Some(5), &[2, 3, 4]),
            ("regexp_instr", 7) => (RegexFunc::Instr, Some(5), &[2, 3, 4, 6]),
            _ => return Err(no_func_overload(name)),
        };
    let mut rargs = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        if int_positions.contains(&i) {
            let (r, t) = resolve(scope, a, Some(ScalarType::Int64), agg, params)?;
            require_int_or_null(&t, name)?;
            rargs.push(r);
        } else {
            let (r, t) = resolve(scope, a, Some(ScalarType::Text), agg, params)?;
            require_text_or_null(&t)?;
            rargs.push(r);
        }
    }
    // Precompile a constant pattern (rargs[1]) once, folding it for a statically-constant `i` flag.
    let insensitive = match flags_idx.map(|i| &rargs[i]) {
        Some(RExpr::ConstText(f)) => Some(f.contains('i')),
        None => Some(false),
        Some(_) => None, // non-constant flags: defer compilation (and the `i` decision) to eval.
    };
    let program = match (&rargs[1], insensitive) {
        (RExpr::ConstText(pat), Some(insensitive)) => {
            let pat = if insensitive {
                let prop = crate::collation::loaded_property();
                crate::collation::fold_lower_simple(pat, prop.as_deref())
            } else {
                pat.clone()
            };
            Some(crate::regex::compile(&pat)?)
        }
        _ => None,
    };
    // A precompiled program carries the one-shot `compile_charged` cost flag mutated on first eval, so
    // a reused plan would under-charge the 2nd+ execute — never cache such a plan.
    if program.is_some() {
        params.uncacheable = true;
    }
    let result = match func {
        RegexFunc::Replace | RegexFunc::Substr => ResolvedType::Text,
        RegexFunc::Match => ResolvedType::Array(Box::new(ResolvedType::Text)),
        RegexFunc::Like => ResolvedType::Bool,
        RegexFunc::Count | RegexFunc::Instr => ResolvedType::Int(ScalarType::Int32),
    };
    Ok((
        RExpr::RegexFunc {
            func,
            args: rargs,
            program,
            compile_charged: std::cell::Cell::new(false),
        },
        result,
    ))
}

/// A numeric regexp_* argument (start/N/endoption/subexpr, regex.md §8b) must be an integer type, or
/// a bare NULL literal (which short-circuits the whole call to NULL at eval). A non-integer operand
/// is 42883 — jed's strict-typing stance rather than PG's implicit text→int cast.
fn require_int_or_null(ty: &ResolvedType, name: &str) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Null => Ok(()),
        _ => Err(no_func_overload(name)),
    }
}

/// Named notation is only valid for a function that declares parameter names. Reject it on any
/// other function — PG's "function ... has no parameter named X" (42883).
fn reject_named(name: &str, arg_names: Option<&[Option<String>]>) -> Result<()> {
    if let Some(names) = arg_names {
        if let Some(Some(pn)) = names.iter().find(|n| n.is_some()) {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("function {name} has no parameter named \"{pn}\""),
            ));
        }
    }
    Ok(())
}

/// The lone scalar-function catalog row of this `name` (e.g. make_interval). Reads the
/// named/default/family metadata for named-notation resolution (functions.md §11) from the
/// generated catalog table (CLAUDE.md §5) rather than re-hardcoding it.
fn scalar_func_desc(name: &str) -> Option<&'static OperatorDesc> {
    OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name)
}

/// The scalar-function catalog row of this `name` with the given `arity` — for a named function
/// overloaded on arity (make_timestamptz: a 6-arg session-zone form + a 7-arg explicit-zone form),
/// so named-notation resolution reads the right slot list (functions.md §11).
fn scalar_func_desc_arity(name: &str, arity: usize) -> Option<&'static OperatorDesc> {
    OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == arity)
}

/// The type context offered to an untyped literal in a function-argument slot of `family`, so it
/// adapts (functions.md §11): an integer slot offers i64, a float slot offers f64 (so a
/// bare `0`/`1.5` becomes f64 for `secs`), a text slot offers text (so a bare `'UTC'` adapts to the
/// `make_timestamptz` `timezone` slot). Other families offer no hint (the literal keeps its default
/// family, and the slot type-check catches a mismatch).
fn family_hint(family: &str) -> Option<ScalarType> {
    match family {
        "integer" => Some(ScalarType::Int64),
        "float" => Some(ScalarType::Float64),
        "text" => Some(ScalarType::Text),
        _ => None,
    }
}

/// Materialize a catalog DEFAULT (an integer-literal string, verify.rb-checked) as an `Expr` so
/// an omitted trailing argument resolves through the normal literal path — adapting to its slot's
/// family (e.g. "0" → f64 for `secs`). functions.md §11.
fn default_expr(lit: &str) -> Expr {
    let n: i64 = lit
        .parse()
        .expect("catalog arg_defaults are integer literals (verify.rb)");
    Expr::Literal(Literal::Int(n))
}

/// Map a call's positional + named arguments onto a function's positional parameter slots,
/// filling omitted trailing slots from `desc.arg_defaults` (PostgreSQL named notation + DEFAULTs,
/// functions.md §11). Returns the positional `Expr` vector of length `desc.arity`. Errors: 42601 a
/// positional arg after a named one (also caught at parse) or a duplicated name; 42883 an unknown
/// parameter name, too many arguments, or a missing non-defaulted slot (no matching overload).
fn normalize_named_args(
    desc: &OperatorDesc,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
) -> Result<Vec<Expr>> {
    let arity = desc.arity as usize;
    let mut slots: Vec<Option<Expr>> = vec![None; arity];
    let mut seen_named = false;
    for (i, a) in args.iter().enumerate() {
        match arg_names.and_then(|ns| ns[i].as_ref()) {
            None => {
                if seen_named {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "positional argument cannot follow named argument",
                    ));
                }
                if i >= arity {
                    return Err(no_func_overload(desc.name)); // too many positional arguments
                }
                slots[i] = Some(a.clone());
            }
            Some(pn) => {
                seen_named = true;
                let idx = desc
                    .arg_names
                    .iter()
                    .position(|p| p.eq_ignore_ascii_case(pn))
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedFunction,
                            format!("function {} has no parameter named \"{pn}\"", desc.name),
                        )
                    })?;
                if slots[idx].is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!("argument name \"{pn}\" used more than once"),
                    ));
                }
                slots[idx] = Some(a.clone());
            }
        }
    }
    let first_defaulted = arity - desc.arg_defaults.len();
    let mut out = Vec::with_capacity(arity);
    for (i, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(e) => out.push(e),
            None if i >= first_defaulted => {
                out.push(default_expr(desc.arg_defaults[i - first_defaulted]))
            }
            None => return Err(no_func_overload(desc.name)), // missing required argument
        }
    }
    Ok(out)
}

/// Resolve `make_interval(years, months, weeks, days, hours, mins, secs)` — the engine's first
/// named + defaulted function (functions.md §11). Normalize named/positional args + defaults onto
/// the seven slots, resolve each with its declared family as the type hint (so a bare numeric
/// literal adapts to the `f64` `secs` slot), and emit a `MakeInterval` node. The arguments
/// keep their families (no promotion); a wrong family in a slot is 42883.
fn resolve_make_interval(
    scope: &Scope,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
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
    let desc = scalar_func_desc("make_interval").expect("make_interval is in the catalog");
    let positional = normalize_named_args(desc, args, arg_names)?;
    let mut rargs = Vec::with_capacity(positional.len());
    for (i, e) in positional.iter().enumerate() {
        let fam = desc.arg_families[i];
        let (r, t) = resolve(scope, e, family_hint(fam), agg, params)?;
        // Type-check the resolved arg against its declared family. A NULL adapts (NULL
        // propagates). A f32 `secs` is read at its own width and widened losslessly to f64
        // at eval (no Cast node — so the cost matches the f64 case and the Go/TS cores).
        let ok = matches!(t, ResolvedType::Null)
            || (fam == "integer" && matches!(t, ResolvedType::Int(_)))
            || (fam == "float" && matches!(t, ResolvedType::Float(_)));
        if !ok {
            return Err(no_func_overload("make_interval"));
        }
        rargs.push(r);
    }
    Ok((
        RExpr::ScalarFunc {
            func: ScalarFunc::MakeInterval,
            args: rargs,
            result: ScalarType::Interval,
        },
        ResolvedType::Interval,
    ))
}

/// Resolve `make_timestamp(year, month, mday, hour, min, sec)` /
/// `make_timestamptz(…[, timezone])` — the named (but un-defaulted) make_interval siblings
/// (functions.md §11). `make_timestamptz` is overloaded on arity: a 6-arg form (interpret in the
/// session zone) and a 7-arg form (an explicit `timezone` text). The right overload is chosen by
/// whether the call supplies a 7th positional argument or names the `timezone` parameter; the
/// chosen catalog row then drives named-notation normalization (unknown name / too many / missing
/// required → 42883, a positional-after-named or duplicate → 42601). Each slot resolves with its
/// declared family as the type hint (a bare numeric literal adapts to the `f64` `sec` slot, a bare
/// string to the `text` `timezone` slot); a wrong family in a slot is 42883.
fn resolve_make_timestamp(
    scope: &Scope,
    name: &str, // "make_timestamp" | "make_timestamptz" (already lowercased)
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
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
    let is_tz = name == "make_timestamptz";
    // Pick the overload: the 7-arg explicit-zone form is selected by a 7th positional argument or a
    // named `timezone`; otherwise the 6-arg form. make_timestamp has only the 6-arg form.
    let arity = if is_tz {
        let positional = args
            .iter()
            .enumerate()
            .filter(|(i, _)| arg_names.and_then(|ns| ns[*i].as_ref()).is_none())
            .count();
        let names_timezone = arg_names.is_some_and(|ns| {
            ns.iter()
                .flatten()
                .any(|n| n.eq_ignore_ascii_case("timezone"))
        });
        if positional > 6 || names_timezone {
            7
        } else {
            6
        }
    } else {
        6
    };
    let desc = scalar_func_desc_arity(name, arity).ok_or_else(|| no_func_overload(name))?;
    let positional = normalize_named_args(desc, args, arg_names)?;
    let mut rargs = Vec::with_capacity(positional.len());
    for (i, e) in positional.iter().enumerate() {
        let fam = desc.arg_families[i];
        let (r, t) = resolve(scope, e, family_hint(fam), agg, params)?;
        // Type-check the resolved arg against its declared family. A NULL adapts (propagates). A
        // f32 `sec` is read at its own width and widened losslessly to f64 at eval (no Cast node, so
        // the cost matches the f64 case and the Go/TS cores).
        let ok = matches!(t, ResolvedType::Null)
            || (fam == "integer" && matches!(t, ResolvedType::Int(_)))
            || (fam == "float" && matches!(t, ResolvedType::Float(_)))
            || (fam == "text" && matches!(t, ResolvedType::Text));
        if !ok {
            return Err(no_func_overload(name));
        }
        rargs.push(r);
    }
    let (func, result) = if is_tz {
        (ScalarFunc::MakeTimestamptz, ScalarType::Timestamptz)
    } else {
        (ScalarFunc::MakeTimestamp, ScalarType::Timestamp)
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

/// Convert `make_interval`'s `secs` (double precision) to a microsecond count: one correctly-
/// rounded multiply, rounded half-away-from-zero to i64 (the engine's one mode — interval.md /
/// float.md §6). A non-finite or out-of-i64-range product traps 22008 (interval out of range),
/// matching PG. The result stays in-contract (the multiply + round are deterministic).
fn f64_to_micros(secs: f64) -> Result<i64> {
    let p = (secs * 1_000_000.0_f64).round(); // round-half-away-from-zero (f64::round)
    // 2^63 = 9_223_372_036_854_775_808.0 is the first f64 strictly above i64::MAX.
    if !p.is_finite() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&p) {
        return Err(EngineError::new(
            SqlState::DatetimeFieldOverflow,
            "interval out of range",
        ));
    }
    Ok(p as i64)
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
    // Pick the overload by argument families, its result type by the catalog `result` code, and
    // its kernel id by name (extensibility.md §5) — replacing the old hand-written (name,
    // arg-types) result match + name→variant match. abs's "promoted" gives the operand's own type
    // (its boundary range-checks for integers; its width for floats, the only `promoted` float fn);
    // round's decimal/integer overloads return numeric, its float overloads f64; the remaining
    // float functions return f64; the uuid extractors/generators return their catalog scalar id.
    let desc = lookup_scalar_overload(name, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = scalar_result_type(desc.result, &tys);
    let func = scalar_func_id(name);
    // Promote float arguments to f64 when the function computes at f64 (every float
    // overload except `abs(f32)`, which keeps its width). The eval then sees one width.
    let widen_args = !matches!(func, ScalarFunc::Abs);
    if widen_args && result == ScalarType::Float64 {
        rargs = rargs
            .into_iter()
            .zip(tys.iter())
            .map(|(r, t)| widen_float_to_f64(r, t))
            .collect();
    }
    Ok((
        RExpr::ScalarFunc {
            func,
            args: rargs,
            result,
        },
        resolved_type_of(result),
    ))
}

/// The 42804 raised when a `VARIADIC` operand is not an array (array-functions.md §12 / §7).
fn variadic_not_array() -> EngineError {
    EngineError::new(
        SqlState::DatatypeMismatch,
        "VARIADIC argument must be an array",
    )
}

/// Resolve a VARIADIC scalar-function call (num_nulls / num_nonnulls — array-functions.md §12).
/// The lone catalog row's last parameter is variadic; the call is EITHER a spread of trailing
/// arguments OR (with the `VARIADIC` keyword) a single array passed directly. Non-strict
/// (`null = "none"`): the resolved node carries no blanket NULL short-circuit. Builds an
/// `RExpr::Variadic` node; the result type is the catalog `result` (i32 here), independent of
/// the arguments.
fn resolve_variadic_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    star: bool,
    variadic: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let desc = scalar_func_desc(name).expect("a variadic function is in the catalog");
    let k = desc.arity as usize; // declared parameter count (the last is variadic)
    let var_family = desc.arg_families[k - 1]; // the variadic element family (last slot)

    let mut rargs = Vec::with_capacity(args.len());
    if variadic {
        // VARIADIC-array form: exactly `k` args (the fixed params + the one array). The fixed
        // params match their concrete families; the last operand MUST be an array (else 42804).
        if args.len() != k {
            return Err(no_func_overload(name));
        }
        for (i, a) in args.iter().enumerate() {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            if i + 1 == k {
                // the variadic (array) operand
                match &t {
                    ResolvedType::Array(elem) => {
                        // "any" accepts any element type; a concrete variadic family must match.
                        if var_family != "any" && !family_matches(var_family, elem) {
                            return Err(no_func_overload(name));
                        }
                    }
                    // A non-array operand (incl. a bare untyped NULL) is 42804 — PG's exact code.
                    _ => return Err(variadic_not_array()),
                }
            } else if !family_matches(desc.arg_families[i], &t) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    } else {
        // Spread form: at least `k` args (so a variadic function needs ≥1 variadic arg —
        // num_nulls() is 42883). The json builders are the exception: a ZERO-arg spread is valid
        // (json_build_array() → [], json_build_object() → {}), so their floor is the fixed-param
        // count (k-1 = 0). The fixed params match their concrete families; every argument from the
        // variadic slot onward matches the variadic element family ("any" ⇒ all).
        let min_args = if json_build_classify(name).is_some() {
            k - 1
        } else {
            k
        };
        if args.len() < min_args {
            return Err(no_func_overload(name));
        }
        for (i, a) in args.iter().enumerate() {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            let slot = if i < k - 1 {
                desc.arg_families[i]
            } else {
                var_family
            };
            if !family_matches(slot, &t) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    }

    let result = scalar_result_type(desc.result, &[]);
    // The json/jsonb builders share the spread/array-form validation above but their own eval node
    // and a json/jsonb result; the count functions (num_nulls/num_nonnulls) keep RExpr::Variadic.
    if let Some((kind, json)) = json_build_classify(name) {
        return Ok((
            RExpr::JsonBuild {
                kind,
                json,
                args: rargs,
                array_form: variadic,
            },
            resolved_type_of(result),
        ));
    }
    Ok((
        RExpr::Variadic {
            func: variadic_func_id(name),
            args: rargs,
            array_form: variadic,
        },
        resolved_type_of(result),
    ))
}

/// Classify a VARIADIC json/jsonb builder name → (kind, is-json). `None` for the count functions.
fn json_build_classify(name: &str) -> Option<(JsonBuildKind, bool)> {
    match name {
        "jsonb_build_array" => Some((JsonBuildKind::Array, false)),
        "json_build_array" => Some((JsonBuildKind::Array, true)),
        "jsonb_build_object" => Some((JsonBuildKind::Object, false)),
        "json_build_object" => Some((JsonBuildKind::Object, true)),
        _ => None,
    }
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
            // `*` with nothing to expand — a FROM-less SELECT — is PostgreSQL's exact error
            // (grammar.md §34). Qualifier-only rels don't count: they are RETURNING's old/new
            // pseudo-relations, and that scope always also carries the real relation.
            if scope.rels.iter().all(|r| r.qualifier_only) {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "SELECT * with no tables specified is not valid",
                ));
            }
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            // USING/NATURAL merged columns come FIRST, in join order (PostgreSQL — grammar.md §15):
            // `SELECT * FROM a JOIN b USING(k)` is `k, <a's other cols>, <b's other cols>`. Each
            // merge emits its surviving-side column; its underlying copies are in `hidden` and so are
            // skipped by the per-relation loop below (which is otherwise the plain `*` expansion).
            for m in &scope.merges {
                let c = scope.column_at(m.index);
                nodes.push(RExpr::Column(m.index));
                names.push(c.name.clone());
                types.push(resolved_type_of_col(&c.ty, scope.catalog));
            }
            // The RETURNING `old`/`new` pseudo-relations are qualifier-only: `*` expands the
            // real relations' columns exactly as before (grammar.md §32).
            for rel in scope.rels.iter().filter(|r| !r.qualifier_only) {
                for (i, c) in rel.table.columns.iter().enumerate() {
                    let idx = rel.offset + i;
                    if scope.hidden.contains(&idx) {
                        continue;
                    }
                    nodes.push(RExpr::Column(idx));
                    names.push(c.name.clone());
                    types.push(resolved_type_of_col(&c.ty, scope.catalog));
                }
            }
            Ok((nodes, names, types))
        }
        SelectItems::Items(items) => {
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            for it in items {
                // `t.*` expands the FROM relation labeled `qualifier` into one output column per
                // column, in catalog order (grammar.md §15) — like bare `*` but for one named
                // relation and mixable with other items. Resolved against the LOCAL scope only
                // (like bare `*`); an unknown label is 42P01, exactly as a qualified column ref.
                if let Expr::QualifiedStar { qualifier } = &it.expr {
                    let want = qualifier.to_ascii_lowercase();
                    let rel = scope
                        .rels
                        .iter()
                        .find(|r| r.label == want)
                        .ok_or_else(|| missing_from_entry(qualifier))?;
                    for (i, c) in rel.table.columns.iter().enumerate() {
                        nodes.push(RExpr::Column(rel.offset + i));
                        names.push(c.name.clone());
                        types.push(resolved_type_of_col(&c.ty, scope.catalog));
                    }
                    continue;
                }
                // `(expr).*` expands a composite base into one output column per field, in
                // declaration order (spec/design/composite.md §S4). The base AST is re-resolved
                // per field (Expr is Clone, RExpr is not) — deterministic, since resolution is
                // pure. An explicit alias on `(c).*` is rejected by PG; we ignore it here (the
                // parser does not attach one to a star item in practice).
                if let Expr::FieldStar { base } = &it.expr {
                    let (_, base_ty) = resolve(scope, base, None, agg, params)?;
                    let fields = match base_ty {
                        ResolvedType::Composite(c) => c.fields,
                        other => {
                            return Err(EngineError::new(
                                SqlState::WrongObjectType,
                                format!(
                                    "column notation .* applied to type {}, which is not a composite type",
                                    other.type_name()
                                ),
                            ));
                        }
                    };
                    for (i, (fname, fty)) in fields.into_iter().enumerate() {
                        let (bn, _) = resolve(scope, base, None, agg, params)?;
                        nodes.push(RExpr::Field {
                            base: Box::new(bn),
                            index: i,
                        });
                        names.push(fname);
                        types.push(fty);
                    }
                    continue;
                }
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
        // spec/design/grammar.md §8). A field selection takes the FIELD name (PG names the
        // output column after the selected field). Any other expression takes `?column?`.
        Expr::FuncCall { name, .. } => name.to_ascii_lowercase(),
        Expr::FieldAccess { field, .. } => field.to_ascii_lowercase(),
        // A subscript takes the base array's name (PG names `a[1]` after `a`); a chained subscript
        // `a[1][2]` recurses to the same base name. A non-column base falls through to `?column?`.
        Expr::Subscript { base, .. } => output_name(scope, base),
        _ => "?column?".to_string(),
    }
}

/// Resolve a bare `ORDER BY` name against the SELECT output columns — PostgreSQL's SQL92 rule that
/// an `ORDER BY` simple name binds an **output** column (an `AS` alias or an item's derived name —
/// grammar.md §8/§10) BEFORE an input column, the opposite of `GROUP BY`'s precedence. Returns the
/// matching select-list item's **expression** (the caller routes it exactly like the same ordinal:
/// a plain column stays on the slot fast path, a computed item is materialized), or `None` when no
/// output name matches (the caller falls back to the FROM scope, the prior behavior). Matching is
/// case-insensitive (§8). Only an explicit list is scanned — with `*` the output names are the scope
/// columns, so the FROM-scope fallback already binds the same column. Two items of the same name
/// with DIFFERENT expressions are ambiguous (`42702`); the same expression twice is not
/// (`SELECT a, a … ORDER BY a`), matching PostgreSQL.
fn order_alias_match<'a>(
    items: &'a SelectItems,
    name: &str,
    scope: &Scope,
) -> Result<Option<&'a Expr>> {
    let SelectItems::Items(items) = items else {
        return Ok(None);
    };
    let mut found: Option<&Expr> = None;
    for it in items {
        let oname = match &it.alias {
            Some(a) => a.clone(),
            None => output_name(scope, &it.expr),
        };
        if !oname.eq_ignore_ascii_case(name) {
            continue;
        }
        match found {
            None => found = Some(&it.expr),
            Some(prev) if *prev != it.expr => {
                return Err(EngineError::new(
                    SqlState::AmbiguousColumn,
                    format!("ORDER BY \"{name}\" is ambiguous"),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(found)
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
        | ResolvedType::Timestamptz
        | ResolvedType::Date
        | ResolvedType::Interval
        | ResolvedType::Float(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => Err(type_error("argument of WHERE must be boolean")),
    }
}

/// Per-statement accumulator of bind-parameter types, inferred from context during resolve
/// (spec/design/api.md §5). `types[i]` is the inferred scalar type of `$(i+1)`; `None` marks a
/// parameter referenced before any context fixed its type. Shared across every clause of a
/// statement (so a `$1` used in both WHERE and the select list unifies), then `finalize`d.
#[derive(Default)]
pub(crate) struct ParamTypes {
    types: Vec<Option<ScalarType>>,
    /// Set during resolution when a node is created that makes the resolved plan un-reusable across
    /// executions: an `RExpr::Subquery` (the uncorrelated-subquery fold rewrites it to a constant
    /// baking in THIS execution's bound params) or a precompiled-regex node (whose one-shot
    /// `compile_charged` cost flag mutates during eval). A prepared statement's plan cache fills only
    /// when this stayed false — flagging at the node's birth is complete regardless of where in the
    /// plan tree it lands (spec/design/api.md §2.4).
    uncacheable: bool,
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
/// widen to the wider (so `$1` works against both an i16 and an i32 column); any other
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

// ================================================================================================
// EXPLAIN — render the planner's chosen plan as a deterministic, cross-core-identical result set
// (spec/design/explain.md). The output is an ordinary query Outcome with three columns:
//
//   depth  i32   the plan node's nesting level (0-based), from a pre-order DFS of the plan tree
//   node   text  the operator label (a fixed vocabulary — the §8 cross-core spelling contract)
//   detail text  the node's attributes (access path, keys, counts); "-" when it has none
//
// Rows are emitted in pre-order, so the row order is deterministic by construction — the corpus
// asserts an EXPLAIN with `nosort`. Every cell is non-empty and free of leading/trailing whitespace
// (indentation is carried by `depth`, never whitespace), so an empty detail uses the "-" sentinel
// (spec/design/explain.md §2). Plain EXPLAIN renders the plan WITHOUT executing the inner statement;
// EXPLAIN ANALYZE runs it and reports the actual accrued cost + row count on an `Analyze` root (§3).
// This mirrors the Go/TS core renderers token-for-token — the shared corpus pins the byte output.
// ================================================================================================

/// Accumulates the rendered plan rows.
#[derive(Default)]
pub(crate) struct ExplainRender {
    rows: Vec<Vec<Value>>,
}

impl ExplainRender {
    /// Append one plan row. An empty detail becomes the "-" sentinel so no cell renders blank.
    fn emit(&mut self, depth: i64, node: impl Into<String>, detail: impl Into<String>) {
        let mut detail = detail.into();
        if detail.is_empty() {
            detail = "-".to_string();
        }
        self.rows.push(vec![
            Value::Int(depth),
            Value::Text(node.into()),
            Value::Text(detail),
        ]);
    }
}

/// Render an INSERT's ON CONFLICT disposition (or "-" when there is none).
fn insert_detail(ins: &Insert) -> String {
    match &ins.on_conflict {
        None => "-".to_string(),
        Some(oc) => match oc.action {
            ConflictAction::DoUpdate { .. } => "on conflict do update".to_string(),
            ConflictAction::DoNothing => "on conflict do nothing".to_string(),
        },
    }
}

/// Render a CTE binding's attributes: its materialization mode (inlined vs materialized — the
/// planner's choice) and whether it is recursive.
fn cte_detail(b: &CteBinding, mode: CteMode) -> String {
    let mut parts = vec![cte_mode_text(mode).to_string()];
    if b.recursive.is_some() {
        parts.push("recursive".to_string());
    }
    parts.join("; ")
}

/// Label a CTE materialization mode.
fn cte_mode_text(m: CteMode) -> &'static str {
    match m {
        CteMode::Materialize => "materialized",
        CteMode::Inline => "inlined",
    }
}

/// Render an Aggregate node's attributes: the grouping-key count, aggregate count, the grouping-set
/// count when there is more than one set, and the HAVING conjunct count.
fn agg_detail(sp: &SelectPlan) -> String {
    let mut parts = vec![format!(
        "groups={} aggs={}",
        sp.group_keys.len(),
        sp.agg_specs.len()
    )];
    if sp.group_sets.len() > 1 {
        parts.push(format!("sets={}", sp.group_sets.len()));
    }
    if let Some(having) = &sp.having {
        parts.push(format!("having:conjuncts={}", conjunct_count(having)));
    }
    parts.join("; ")
}

/// Render a Nested Loop node's attributes: the join kind and the ON predicate's conjunct count (a
/// CROSS join has no ON).
fn join_detail(j: &PlanJoin) -> String {
    let kind = join_kind_text(j.kind);
    match &j.on {
        None => kind.to_string(),
        Some(on) => format!("{kind}; on:conjuncts={}", conjunct_count(on)),
    }
}

/// The label for a join kind.
fn join_kind_text(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "inner",
        JoinKind::Cross => "cross",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
    }
}

/// The node label for a set-operation kind.
fn set_op_node_name(op: SetOpKind) -> &'static str {
    match op {
        SetOpKind::Union => "Union",
        SetOpKind::Intersect => "Intersect",
        SetOpKind::Except => "Except",
    }
}

/// Render a set operation's ALL / DISTINCT disposition.
fn set_op_detail(all: bool) -> &'static str {
    if all { "all" } else { "distinct" }
}

/// Append an elided-ORDER-BY note to a node's detail (replacing a "-" sentinel).
fn with_note(detail: impl Into<String>, note: &str) -> String {
    let detail = detail.into();
    if note.is_empty() {
        return detail;
    }
    if detail.is_empty() || detail == "-" {
        return format!("ordered: {note}");
    }
    format!("{detail}; ordered: {note}")
}

/// Render a merged OR / IN-list point-set bound's const-sources as `col in (a, b, c)` (cost.md §3
/// "OR / IN-list"), in source order (the plan-time order, before the exec-time encode / dedup /
/// sort) — deterministic across cores. Each source renders via [`render_bound_src`] (a bind param
/// as `$N`, a correlated column as `outer`, a literal via its token).
fn render_key_set(col: &str, srcs: &[BoundSrc]) -> String {
    let parts: Vec<String> = srcs.iter().map(render_bound_src).collect();
    format!("{col} in ({})", parts.join(", "))
}

/// Render a primary-key bound's terms as `col <op> <src>` conjuncts joined by " and " — e.g.
/// `id = $1`, `id >= 5 and id < 10`.
fn render_bound_terms(col: &str, terms: &[BoundTerm]) -> String {
    terms
        .iter()
        .map(|t| format!("{col} {} {}", bound_op_text(t.op), render_bound_src(&t.src)))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The symbol for a bound comparison operator.
fn bound_op_text(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::Le => "<=",
        CmpOp::Ge => ">=",
    }
}

/// Render a bound's const-source operand: a bind parameter as `$N` (1-based), a correlated
/// outer-column reference as `outer`, or a literal. Integer / boolean / decimal literals render
/// deterministically; a text literal renders verbatim unless it contains a newline (which would split
/// the cell), in which case `<text>`; every other constant type renders `<value>` for now (a later
/// slice widens this). A `float` bound source cannot arise here — float keys do not push down, so the
/// determinism-ledger `<float>` token the Go/TS renderers reserve has no [`BoundSrc`] analogue.
fn render_bound_src(src: &BoundSrc) -> String {
    match src {
        BoundSrc::Param(idx) => format!("${}", idx + 1),
        BoundSrc::Outer { .. } => "outer".to_string(),
        // An index-nested-loop bound source — a column of an earlier join relation resolved per
        // outer row (cost.md §3 "JOIN"). Rendered generically (the global column index is not a
        // user-facing name, like the correlated `outer` case above).
        BoundSrc::Sibling(_) => "join".to_string(),
        BoundSrc::Int(n) => n.to_string(),
        BoundSrc::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        BoundSrc::Decimal(d) => d.render(),
        BoundSrc::Text(s) => {
            if s.contains(['\n', '\r']) {
                "<text>".to_string()
            } else {
                format!("'{s}'")
            }
        }
        BoundSrc::Uuid(_)
        | BoundSrc::Timestamp(_)
        | BoundSrc::Date(_)
        | BoundSrc::Bytea(_)
        | BoundSrc::Interval(_)
        | BoundSrc::Null => "<value>".to_string(),
    }
}

/// Count the top-level AND conjuncts of a residual filter (a deterministic integer — the plan text
/// carries the count, not the expression itself; a full expression printer is a later slice,
/// spec/design/explain.md §5).
fn conjunct_count(e: &RExpr) -> i64 {
    match e {
        RExpr::And(l, r) => conjunct_count(l) + conjunct_count(r),
        _ => 1,
    }
}

/// Render a Limit node's `limit=N` / `offset=M` attributes (an absent side is omitted).
fn limit_detail(limit: Option<i64>, offset: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(l) = limit {
        parts.push(format!("limit={l}"));
    }
    if let Some(o) = offset {
        parts.push(format!("offset={o}"));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

/// Count the set entries in a touched-set mask.
fn count_true(mask: &[bool]) -> usize {
    mask.iter().filter(|&&b| b).count()
}

/// Whether a statement mutates the database (so autocommit must capture + durably persist it,
/// and a READ ONLY transaction must reject it — spec/design/transactions.md §4.1/§4.3). Reads
/// (`SELECT`, set operations) and transaction control run against the committed state / handle
/// state with no data mutation.
/// Map a `serial` pseudo-type name to its underlying integer scalar (spec/design/sequences.md §12) —
/// `serial`/`serial4` → i32, `bigserial`/`serial8` → i64, `smallserial`/`serial2` → i16. `None` for
/// any other name. Recognized **only** in a CREATE TABLE column-type position (the one caller); the
/// match is case-insensitive (the parser passes the type name verbatim).
fn serial_pseudo_type(name: &str) -> Option<ScalarType> {
    match name.to_ascii_lowercase().as_str() {
        "serial" | "serial4" => Some(ScalarType::Int32),
        "bigserial" | "serial8" => Some(ScalarType::Int64),
        "smallserial" | "serial2" => Some(ScalarType::Int16),
        _ => None,
    }
}

/// Resolve a parsed `SeqOptions` set into a validated `SequenceDef` (spec/design/sequences.md §1/§14),
/// shared by `CREATE SEQUENCE` and an IDENTITY column's `( seq_options )` (§13). The `AS` type (or the
/// `serial`/identity-supplied default) sets the default + validated bounds; then validates INCREMENT
/// (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type range, MINVALUE ≤ MAXVALUE, and START in
/// `[min, max]` (each `22023`). A fresh sequence starts with `last_value = start`, `is_called = false`.
/// `owned_by` carries the IDENTITY / `serial` owner link (`None` for a plain `CREATE SEQUENCE`).
fn build_sequence_def(
    name: &str,
    options: &SeqOptions,
    owned_by: Option<SeqOwner>,
) -> Result<SequenceDef> {
    // The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
    let dtype = match &options.data_type {
        Some(tn) => SeqDataType::from_type_name(tn).ok_or_else(|| {
            EngineError::new(
                SqlState::InvalidParameterValue,
                "sequence type must be smallint, integer, or bigint".to_string(),
            )
        })?,
        None => SeqDataType::BigInt,
    };
    let (type_min, type_max) = dtype.range();
    let increment = options.increment.unwrap_or(1);
    if increment == 0 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "INCREMENT must not be zero".to_string(),
        ));
    }
    let cache = options.cache.unwrap_or(1);
    if cache < 1 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("CACHE ({cache}) must be greater than zero"),
        ));
    }
    let (def_min, def_max) = dtype.default_bounds(increment);
    // An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
    // BEFORE the MIN > MAX consistency check (§14.2).
    if let Some(Some(v)) = options.max_value {
        if v > type_max {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!(
                    "MAXVALUE ({v}) is out of range for sequence data type {}",
                    dtype.pg_name()
                ),
            ));
        }
    }
    if let Some(Some(v)) = options.min_value {
        if v < type_min {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!(
                    "MINVALUE ({v}) is out of range for sequence data type {}",
                    dtype.pg_name()
                ),
            ));
        }
    }
    // `Some(Some(v))` MINVALUE v / `Some(None)` NO MINVALUE / `None` unset → the type default.
    let min_value = match options.min_value {
        Some(Some(v)) => v,
        Some(None) | None => def_min,
    };
    let max_value = match options.max_value {
        Some(Some(v)) => v,
        Some(None) | None => def_max,
    };
    // PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
    // previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
    if min_value >= max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("MINVALUE ({min_value}) must be less than MAXVALUE ({max_value})"),
        ));
    }
    // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
    let start = options
        .start
        .unwrap_or(if increment < 0 { max_value } else { min_value });
    seq_bound_check_start(start, min_value, max_value)?;
    Ok(SequenceDef {
        name: name.to_string(),
        increment,
        min_value,
        max_value,
        start,
        cache,
        cycle: options.cycle.unwrap_or(false),
        last_value: start,
        is_called: false,
        owned_by,
    })
}

/// PG's START-in-bounds cross-check (`init_params`): `start ∈ [min, max]`, else 22023 with PG's
/// wording. Shared by `CREATE` (build_sequence_def) and `ALTER` (apply_seq_alter).
fn seq_bound_check_start(start: i64, min_value: i64, max_value: i64) -> Result<()> {
    if start < min_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("START value ({start}) cannot be less than MINVALUE ({min_value})"),
        ));
    }
    if start > max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("START value ({start}) cannot be greater than MAXVALUE ({max_value})"),
        ));
    }
    Ok(())
}

/// PG's last_value (RESTART) cross-check (`init_params`): the post-edit `last_value ∈ [min, max]`,
/// else 22023. PG uses the "RESTART value …" wording even when no `RESTART` was written (§15.2).
fn seq_bound_check_last(last_value: i64, min_value: i64, max_value: i64) -> Result<()> {
    if last_value < min_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("RESTART value ({last_value}) cannot be less than MINVALUE ({min_value})"),
        ));
    }
    if last_value > max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("RESTART value ({last_value}) cannot be greater than MAXVALUE ({max_value})"),
        ));
    }
    Ok(())
}

/// Re-edit an existing `SequenceDef` per `ALTER SEQUENCE s <options>` (spec/design/sequences.md §15.2)
/// — PG's `init_params` with `isInit = false`. Only the **written** options change; `last_value`/
/// `is_called` are preserved unless `restart` is given. `restart` is `None` (no `RESTART`),
/// `Some(None)` (bare `RESTART` → the stored `START`), or `Some(Some(n))` (`RESTART WITH n`). The
/// value type is not persisted (§14.4), so `NO MINVALUE`/`NO MAXVALUE` reset the open direction to the
/// bigint bound and an explicit bound is range-checked only by `i64` — a documented divergence for a
/// typed sequence. `data_type` must be `None` (the caller rejects `AS` as 0A000 first).
fn apply_seq_alter(
    existing: &SequenceDef,
    options: &SeqOptions,
    restart: Option<Option<i64>>,
) -> Result<SequenceDef> {
    debug_assert!(
        options.data_type.is_none(),
        "ALTER ... AS is rejected 0A000 by the caller"
    );
    let mut def = existing.clone();
    if let Some(inc) = options.increment {
        if inc == 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "INCREMENT must not be zero".to_string(),
            ));
        }
        def.increment = inc;
    }
    if let Some(c) = options.cache {
        if c < 1 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("CACHE ({c}) must be greater than zero"),
            ));
        }
        def.cache = c;
    }
    // `NO MINVALUE`/`NO MAXVALUE` recompute the default for the (possibly new) INCREMENT sign — but
    // against the bigint range, since the value type is not persisted (§14.4). An explicit bound is
    // taken as written (i64-bounded only). An unwritten bound is preserved (PG keeps it even when the
    // INCREMENT sign flips — sequences.md §15.2).
    let (def_min, def_max) = SeqDataType::BigInt.default_bounds(def.increment);
    match options.min_value {
        Some(Some(v)) => def.min_value = v,
        Some(None) => def.min_value = def_min,
        None => {}
    }
    match options.max_value {
        Some(Some(v)) => def.max_value = v,
        Some(None) => def.max_value = def_max,
        None => {}
    }
    if def.min_value >= def.max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!(
                "MINVALUE ({}) must be less than MAXVALUE ({})",
                def.min_value, def.max_value
            ),
        ));
    }
    if let Some(s) = options.start {
        def.start = s;
    }
    // Cross-check 1: START ∈ [min, max].
    seq_bound_check_start(def.start, def.min_value, def.max_value)?;
    // RESTART (applied last, before the last_value cross-check).
    match restart {
        Some(Some(n)) => {
            def.last_value = n;
            def.is_called = false;
        }
        Some(None) => {
            def.last_value = def.start;
            def.is_called = false;
        }
        None => {}
    }
    // Cross-check 2: the preserved/restarted last_value ∈ [min, max].
    seq_bound_check_last(def.last_value, def.min_value, def.max_value)?;
    if let Some(c) = options.cycle {
        def.cycle = c;
    }
    Ok(def)
}

pub(crate) fn stmt_is_write(stmt: &Statement) -> bool {
    // EXPLAIN is a read: plain EXPLAIN plans without executing (even of a DML inner — it never
    // mutates). Only EXPLAIN ANALYZE runs the inner statement, so it is a write iff the inner is
    // (spec/design/explain.md §3).
    if let Statement::Explain { analyze, inner } = stmt {
        return *analyze && stmt_is_write(inner);
    }
    matches!(
        stmt,
        Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::CreateType(_)
            | Statement::DropType(_)
            | Statement::CreateSequence(_)
            | Statement::AlterSequence(_)
            | Statement::DropSequence(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
    )
    // A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE
    // effects — writable-cte.md): it must take the write gate, accumulate into `working`, and commit.
    || matches!(stmt, Statement::With(wq) if with_has_dml(wq))
    // A read-shaped statement that calls a sequence-mutating function (nextval/setval) IS a write
    // (spec/design/sequences.md §4): it must take the write gate, stage the advance, and commit
    // (autocommit) — and is 25006 in a READ ONLY transaction, exactly like any other write.
    || stmt_calls_seq_mutator(stmt)
}

/// Whether `stmt`'s expression trees contain a sequence-MUTATING function call (`nextval`; in S2,
/// `setval`) anywhere — which makes an otherwise read-shaped statement a write (sequences.md §4).
/// Only the **read-shaped** statements need checking: INSERT/UPDATE/DELETE/DDL are already writes
/// (the `matches!` in [`stmt_is_write`] short-circuits before this), and an INSERT `VALUES` slot is
/// literal-only (no function call). `currval` is a pure read and is NOT counted. The `Expr` walk is
/// exhaustive (the compiler enforces it), so no expression position is missed.
fn stmt_calls_seq_mutator(stmt: &Statement) -> bool {
    match stmt {
        Statement::Select(s) => select_calls_seq_mutator(s),
        Statement::SetOp(so) => setop_calls_seq_mutator(so),
        Statement::With(w) => {
            w.ctes.iter().any(|c| cte_body_calls_seq_mutator(&c.body))
                || cte_body_calls_seq_mutator(&w.body)
        }
        _ => false,
    }
}

/// Whether a `cte_body` calls a sequence-mutating function. A query body delegates to the query
/// walk; a data-modifying body already makes the `WITH` a write (via [`with_has_dml`]), so this is
/// not reached for it — it is treated as a write regardless (writable-cte.md).
fn cte_body_calls_seq_mutator(body: &CteBody) -> bool {
    match body {
        CteBody::Query(q) => query_calls_seq_mutator(q),
        _ => true,
    }
}

fn query_calls_seq_mutator(qe: &QueryExpr) -> bool {
    match qe {
        QueryExpr::Select(s) => select_calls_seq_mutator(s),
        QueryExpr::SetOp(so) => setop_calls_seq_mutator(so),
        // A nested `WITH`'s CTE bodies and main body may call a sequence mutator (cte.md §7).
        QueryExpr::With(we) => {
            we.ctes.iter().any(|c| cte_body_calls_seq_mutator(&c.body))
                || query_calls_seq_mutator(&we.body)
        }
    }
}

fn setop_calls_seq_mutator(so: &SetOp) -> bool {
    query_calls_seq_mutator(&so.lhs) || query_calls_seq_mutator(&so.rhs)
}

fn select_calls_seq_mutator(s: &Select) -> bool {
    let item_calls = match &s.items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|i| expr_calls_seq_mutator(&i.expr)),
    };
    item_calls
        || s.from.as_ref().is_some_and(table_ref_calls)
        || s.joins
            .iter()
            .any(|j| table_ref_calls(&j.table) || j.on.as_ref().is_some_and(expr_calls_seq_mutator))
        || s.filter.as_ref().is_some_and(expr_calls_seq_mutator)
        || s.group_by.iter().any(|item| {
            let mut found = false;
            item.for_each_expr(&mut |e| found |= expr_calls_seq_mutator(e));
            found
        })
        || s.having.as_ref().is_some_and(expr_calls_seq_mutator)
}

fn table_ref_calls(t: &TableRef) -> bool {
    t.args
        .as_ref()
        .is_some_and(|a| a.iter().any(expr_calls_seq_mutator))
        || t.subquery
            .as_ref()
            .is_some_and(|q| query_calls_seq_mutator(q))
        || t.values
            .as_ref()
            .is_some_and(|rows| rows.iter().flatten().any(expr_calls_seq_mutator))
}

/// Exhaustive over `Expr` (the compiler enforces it): true iff the tree contains a sequence-
/// mutating call (`nextval` or `setval`).
fn expr_calls_seq_mutator(e: &Expr) -> bool {
    match e {
        Expr::FuncCall { name, args, .. } => {
            name.eq_ignore_ascii_case("nextval")
                || name.eq_ignore_ascii_case("setval")
                || args.iter().any(expr_calls_seq_mutator)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Row(es) | Expr::Array(es) => es.iter().any(expr_calls_seq_mutator),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_calls_seq_mutator(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_calls_seq_mutator(base)
                || subscripts.iter().any(|s| match s {
                    SubscriptSpec::Index(x) => expr_calls_seq_mutator(x),
                    SubscriptSpec::Slice(lo, hi) => {
                        lo.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                            || hi.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                    }
                })
        }
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. } => expr_calls_seq_mutator(inner),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_calls_seq_mutator(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            expr_calls_seq_mutator(ctx) || expr_calls_seq_mutator(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_calls_seq_mutator(lhs) || list.iter().any(expr_calls_seq_mutator)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(lo) || expr_calls_seq_mutator(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                || whens
                    .iter()
                    .any(|(c, r)| expr_calls_seq_mutator(c) || expr_calls_seq_mutator(r))
                || els.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
        }
        Expr::ScalarSubquery(q) | Expr::Exists(q) => query_calls_seq_mutator(q),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            expr_calls_seq_mutator(lhs) || query_calls_seq_mutator(query)
        }
        Expr::Quantified { lhs, array, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(array)
        }
    }
}

/// The privilege requirements collected from one statement (spec/design/session.md §5.3): the
/// per-table privileges, the named functions (each needs `EXECUTE`), and whether the statement is
/// DDL (gated by `allow_ddl`). Collected by an exhaustive AST walk (the `Expr` arm is compiler-
/// enforced exhaustive, mirroring [`expr_calls_seq_mutator`]).
#[derive(Default)]
pub(crate) struct PrivReq {
    /// `(table name, required privilege)` in source-walk order; deduplication is unnecessary (the
    /// check is idempotent and a fully-permissive session never reaches the walk).
    tables: Vec<(String, Privilege)>,
    /// Named functions called (each requires `EXECUTE`), in source-walk order.
    functions: Vec<String>,
    /// Whether the statement is DDL (CREATE / DROP / ALTER) — gated by `allow_ddl`.
    is_ddl: bool,
    /// Whether the DDL targets a SESSION-LOCAL temporary table (`CREATE TEMP TABLE`) — gated by
    /// `allow_temp_ddl` instead of `allow_ddl` (spec/design/temp-tables.md §5). Set for a `CREATE
    /// TEMP`; a `DROP` is classified by resolving the name in `check_privileges`.
    is_temp_ddl: bool,
}

impl PrivReq {
    fn need_table(&mut self, name: &str, p: Privilege) {
        self.tables.push((name.to_string(), p));
    }
    fn need_function(&mut self, name: &str) {
        self.functions.push(name.to_string());
    }
}

/// Collect the privilege requirements of `stmt` (spec/design/session.md §5.3). Transaction control
/// carries none (it is handled before dispatch); DDL just sets `is_ddl`.
fn collect_stmt_privs(stmt: &Statement, req: &mut PrivReq) {
    let locals = HashSet::new();
    match stmt {
        Statement::CreateTable(ct) => {
            req.is_ddl = true;
            // A temp table's DDL is gated by the temp-scoped split of `allow_ddl` (temp-tables.md §5):
            // `allow_temp_ddl` for a session-local temp table.
            req.is_temp_ddl = ct.temp;
        }
        Statement::DropTable(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::CreateType(_)
        | Statement::DropType(_)
        | Statement::CreateSequence(_)
        | Statement::DropSequence(_)
        | Statement::AlterSequence(_) => req.is_ddl = true,
        Statement::Insert(ins) => collect_insert_privs(ins, req, &locals),
        Statement::Select(sel) => collect_select_privs(sel, req, &locals),
        Statement::SetOp(so) => collect_setop_privs(so, req, &locals),
        Statement::With(wq) => collect_with_privs(wq, req, &locals),
        Statement::Update(upd) => collect_update_privs(upd, req, &locals),
        Statement::Delete(del) => collect_delete_privs(del, req, &locals),
        // EXPLAIN requires the inner statement's privileges (EXPLAIN INSERT needs INSERT, matching
        // PG). Plain EXPLAIN never executes, but authorization is checked on the inner regardless
        // (spec/design/explain.md §1).
        Statement::Explain { inner, .. } => collect_stmt_privs(inner, req),
        Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {}
    }
}

fn collect_insert_privs(ins: &Insert, req: &mut PrivReq, locals: &HashSet<String>) {
    // The write target needs INSERT. A bare `INSERT … VALUES` reads nothing (the slots are literals
    // / params), so it needs only INSERT; an `INSERT … SELECT` source needs SELECT on its tables.
    req.need_table(&ins.table, Privilege::Insert);
    if let InsertSource::Select(sel) = &ins.source {
        collect_select_privs(sel, req, locals);
    }
    if let Some(oc) = &ins.on_conflict {
        if let ConflictAction::DoUpdate {
            assignments,
            filter,
        } = &oc.action
        {
            for a in assignments {
                collect_expr_privs(&a.value, req, locals);
            }
            if let Some(f) = filter {
                collect_expr_privs(f, req, locals);
            }
        }
    }
    collect_items_privs(&ins.returning, req, locals);
}

fn collect_update_privs(upd: &Update, req: &mut PrivReq, locals: &HashSet<String>) {
    req.need_table(&upd.table, Privilege::Update);
    // SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
    // referencing assignment RHS (a constant-only `SET a = 1` with no WHERE/RETURNING reads nothing).
    let reads = upd.filter.is_some()
        || upd.returning.is_some()
        || upd.assignments.iter().any(|a| expr_reads_columns(&a.value));
    if reads {
        req.need_table(&upd.table, Privilege::Select);
    }
    for a in &upd.assignments {
        collect_expr_privs(&a.value, req, locals);
    }
    if let Some(f) = &upd.filter {
        collect_expr_privs(f, req, locals);
    }
    collect_items_privs(&upd.returning, req, locals);
}

fn collect_delete_privs(del: &Delete, req: &mut PrivReq, locals: &HashSet<String>) {
    req.need_table(&del.table, Privilege::Delete);
    // DELETE reads the target's columns through a WHERE or a RETURNING.
    if del.filter.is_some() || del.returning.is_some() {
        req.need_table(&del.table, Privilege::Select);
    }
    if let Some(f) = &del.filter {
        collect_expr_privs(f, req, locals);
    }
    collect_items_privs(&del.returning, req, locals);
}

fn collect_query_privs(qe: &QueryExpr, req: &mut PrivReq, locals: &HashSet<String>) {
    match qe {
        QueryExpr::Select(s) => collect_select_privs(s, req, locals),
        QueryExpr::SetOp(so) => collect_setop_privs(so, req, locals),
        // A nested `WITH` establishes its own CTE scope (spec/design/cte.md §7): the enclosing
        // locals are NOT inherited (an enclosing CTE name resolves to a base table inside, so it is
        // privilege-checked), and the nested CTE names shadow base tables only within this node.
        QueryExpr::With(we) => {
            let mut scope = HashSet::new();
            for cte in &we.ctes {
                collect_cte_body_privs(&cte.body, req, &scope);
                scope.insert(cte.name.to_ascii_lowercase());
            }
            collect_query_privs(&we.body, req, &scope);
        }
    }
}

fn collect_setop_privs(so: &SetOp, req: &mut PrivReq, locals: &HashSet<String>) {
    collect_query_privs(&so.lhs, req, locals);
    collect_query_privs(&so.rhs, req, locals);
}

fn collect_with_privs(wq: &WithQuery, req: &mut PrivReq, locals: &HashSet<String>) {
    // A CTE name shadows a base table inside the WITH (a `FROM <cte>` is not a catalog object), so
    // it is added to the local scope and never privilege-checked. Forward-only visibility: each CTE
    // body sees the CTE names declared before it. A data-modifying body / primary needs the write
    // privilege on its target table (writable-cte.md).
    let mut scope = locals.clone();
    for cte in &wq.ctes {
        collect_cte_body_privs(&cte.body, req, &scope);
        scope.insert(cte.name.to_ascii_lowercase());
    }
    collect_cte_body_privs(&wq.body, req, &scope);
}

/// Collect the privilege requirements of a `cte_body` — a query, or a data-modifying statement
/// (spec/design/writable-cte.md) which needs the write privilege on its target.
fn collect_cte_body_privs(body: &CteBody, req: &mut PrivReq, locals: &HashSet<String>) {
    match body {
        CteBody::Query(q) => collect_query_privs(q, req, locals),
        CteBody::Insert(ins) => collect_insert_privs(ins, req, locals),
        CteBody::Update(upd) => collect_update_privs(upd, req, locals),
        CteBody::Delete(del) => collect_delete_privs(del, req, locals),
    }
}

fn collect_select_privs(s: &Select, req: &mut PrivReq, locals: &HashSet<String>) {
    if let Some(from) = &s.from {
        collect_table_ref_privs(from, req, locals);
    }
    for j in &s.joins {
        collect_table_ref_privs(&j.table, req, locals);
        if let Some(on) = &j.on {
            collect_expr_privs(on, req, locals);
        }
    }
    if let SelectItems::Items(items) = &s.items {
        for it in items {
            collect_expr_privs(&it.expr, req, locals);
        }
    }
    if let Some(f) = &s.filter {
        collect_expr_privs(f, req, locals);
    }
    for item in &s.group_by {
        item.for_each_expr(&mut |g| collect_expr_privs(g, req, locals));
    }
    if let Some(h) = &s.having {
        collect_expr_privs(h, req, locals);
    }
}

fn collect_table_ref_privs(t: &TableRef, req: &mut PrivReq, locals: &HashSet<String>) {
    if let Some(args) = &t.args {
        // A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
        req.need_function(&t.name);
        for a in args {
            collect_expr_privs(a, req, locals);
        }
    } else if let Some(sub) = &t.subquery {
        collect_query_privs(sub, req, locals);
    } else if let Some(rows) = &t.values {
        for e in rows.iter().flatten() {
            collect_expr_privs(e, req, locals);
        }
    } else if !locals.contains(&t.name.to_ascii_lowercase()) {
        // A base-table reference (not a CTE / derived-table label) — needs SELECT.
        req.need_table(&t.name, Privilege::Select);
    }
}

fn collect_items_privs(items: &Option<SelectItems>, req: &mut PrivReq, locals: &HashSet<String>) {
    if let Some(SelectItems::Items(list)) = items {
        for it in list {
            collect_expr_privs(&it.expr, req, locals);
        }
    }
}

/// Exhaustive over `Expr` (compiler-enforced, mirroring [`expr_calls_seq_mutator`]): collect every
/// named function call (`EXECUTE`) and walk every subquery (its tables need `SELECT`).
fn collect_expr_privs(e: &Expr, req: &mut PrivReq, locals: &HashSet<String>) {
    match e {
        Expr::FuncCall { name, args, .. } => {
            req.need_function(name);
            for a in args {
                collect_expr_privs(a, req, locals);
            }
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => {}
        Expr::Row(es) | Expr::Array(es) => {
            for x in es {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            collect_expr_privs(base, req, locals)
        }
        // `t.*` names a relation already in FROM — its SELECT privilege is required by the FROM
        // clause itself, so the star adds no new function/table privilege here.
        Expr::QualifiedStar { .. } => {}
        Expr::Subscript { base, subscripts } => {
            collect_expr_privs(base, req, locals);
            for s in subscripts {
                match s {
                    SubscriptSpec::Index(x) => collect_expr_privs(x, req, locals),
                    SubscriptSpec::Slice(lo, hi) => {
                        if let Some(x) = lo {
                            collect_expr_privs(x, req, locals);
                        }
                        if let Some(x) = hi {
                            collect_expr_privs(x, req, locals);
                        }
                    }
                }
            }
        }
        Expr::Cast { inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => collect_expr_privs(inner, req, locals),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => collect_expr_privs(operand, req, locals),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            collect_expr_privs(ctx, req, locals);
            collect_expr_privs(path, req, locals);
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(rhs, req, locals);
        }
        Expr::In { lhs, list, .. } => {
            collect_expr_privs(lhs, req, locals);
            for x in list {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::Between { lhs, lo, hi, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(lo, req, locals);
            collect_expr_privs(hi, req, locals);
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(x) = operand {
                collect_expr_privs(x, req, locals);
            }
            for (c, r) in whens {
                collect_expr_privs(c, req, locals);
                collect_expr_privs(r, req, locals);
            }
            if let Some(x) = els {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::ScalarSubquery(q) | Expr::Exists(q) => collect_query_privs(q, req, locals),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_query_privs(query, req, locals);
        }
        Expr::Quantified { lhs, array, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(array, req, locals);
        }
    }
}

/// Whether `e` reads a stored column or a subquery's rows — the trigger for an UPDATE's `SELECT`
/// requirement on its target (spec/design/session.md §5.3). A column reference (`Column` /
/// `QualifiedColumn` / a field/subscript over one) or any subquery counts; a pure constant /
/// parameter expression does not. Exhaustive over `Expr` (compiler-enforced).
fn expr_reads_columns(e: &Expr) -> bool {
    match e {
        Expr::Column(_) | Expr::QualifiedColumn { .. } => true,
        Expr::ScalarSubquery(_) | Expr::Exists(_) => true,
        Expr::Literal(_) | Expr::TypedLiteral { .. } | Expr::Param(_) => false,
        Expr::Row(es) | Expr::Array(es) => es.iter().any(expr_reads_columns),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_reads_columns(base),
        // `t.*` reads the relation's columns (e.g. `RETURNING t.*`).
        Expr::QualifiedStar { .. } => true,
        Expr::Subscript { base, subscripts } => {
            expr_reads_columns(base)
                || subscripts.iter().any(|s| match s {
                    SubscriptSpec::Index(x) => expr_reads_columns(x),
                    SubscriptSpec::Slice(lo, hi) => {
                        lo.as_ref().is_some_and(|x| expr_reads_columns(x))
                            || hi.as_ref().is_some_and(|x| expr_reads_columns(x))
                    }
                })
        }
        Expr::Cast { inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => expr_reads_columns(inner),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_reads_columns(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_reads_columns(ctx) || expr_reads_columns(path),
        Expr::FuncCall { args, .. } => args.iter().any(expr_reads_columns),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => expr_reads_columns(lhs) || expr_reads_columns(rhs),
        Expr::In { lhs, list, .. } => {
            expr_reads_columns(lhs) || list.iter().any(expr_reads_columns)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            expr_reads_columns(lhs) || expr_reads_columns(lo) || expr_reads_columns(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_ref().is_some_and(|x| expr_reads_columns(x))
                || whens
                    .iter()
                    .any(|(c, r)| expr_reads_columns(c) || expr_reads_columns(r))
                || els.as_ref().is_some_and(|x| expr_reads_columns(x))
        }
        Expr::InSubquery { .. } | Expr::QuantifiedSubquery { .. } => true,
        Expr::Quantified { lhs, array, .. } => expr_reads_columns(lhs) || expr_reads_columns(array),
    }
}

/// A short label for a statement kind, for the 25006 read-only-violation message (the message
/// text is informational — never matched; spec/design/conformance.md §2).
fn stmt_kind(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable(_) => "DROP TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        Statement::CreateType(_) => "CREATE TYPE",
        Statement::DropType(_) => "DROP TYPE",
        Statement::CreateSequence(_) => "CREATE SEQUENCE",
        Statement::AlterSequence(_) => "ALTER SEQUENCE",
        Statement::DropSequence(_) => "DROP SEQUENCE",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Select(_) | Statement::SetOp(_) | Statement::With(_) => "SELECT",
        Statement::Explain { .. } => "EXPLAIN",
        Statement::Begin { .. } => "BEGIN",
        Statement::Commit => "COMMIT",
        Statement::Rollback => "ROLLBACK",
    }
}

/// The resolved (static) type of a column of (possibly composite) declared type `ty`, resolving a
/// composite reference against the database's type catalog (spec/design/composite.md §5). Recurses
/// for nested composites; the lookup always succeeds (`validate_composite_types` proved it).
fn resolved_type_of_col(ty: &Type, db: &Engine) -> ResolvedType {
    match ty {
        Type::Scalar(s) => resolved_type_of(*s),
        Type::Composite(r) => {
            let def = db
                .composite_type(&r.name)
                .expect("composite type reference resolved at load / CREATE TYPE");
            let fields = def
                .fields
                .iter()
                .map(|f| (f.name.clone(), resolved_type_of_col(&f.ty, db)))
                .collect();
            ResolvedType::Composite(Box::new(CompositeRType {
                name: Some(def.name.clone()),
                fields,
            }))
        }
        Type::Array(elem) => ResolvedType::Array(Box::new(resolved_type_of_col(elem, db))),
        Type::Range(elem) => ResolvedType::Range(Box::new(resolved_type_of_col(elem, db))),
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
    } else if ty.is_interval() {
        ResolvedType::Interval
    } else if ty.is_date() {
        ResolvedType::Date
    } else if ty.is_json() {
        ResolvedType::Json
    } else if ty.is_jsonb() {
        ResolvedType::Jsonb
    } else if ty.is_float() {
        ResolvedType::Float(ty)
    } else {
        ResolvedType::Int(ty)
    }
}

/// Resolve one `Expr` into an `RExpr` plus its static type, against the FROM `scope`. `ctx`
/// is the type an untyped integer literal should adapt to (spec/design/types.md §6); `None`
/// defaults a bare literal to i64. A column reference resolves to a flat row index via the
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
            let ty = resolved_type_of_col(&scope.column_of(r).ty, scope.catalog);
            Ok((RExpr::OuterColumn { level, index }, ty))
        }
    }
}

/// Resolve a composite field selection `base.field` (spec/design/composite.md §S4) given the
/// already-resolved `base` node and its static type: `base` must be composite — else 42809
/// (wrong_object_type, PG's "column notation applied to non-composite") — and `field` must name
/// one of its fields case-insensitively (PG folds the identifier), else 42703 (undefined_column).
/// Returns the `RExpr::Field` node carrying the fixed field ordinal, plus the field's static type.
fn resolve_field_of(
    base_node: RExpr,
    base_ty: ResolvedType,
    field: &str,
) -> Result<(RExpr, ResolvedType)> {
    let c = match base_ty {
        ResolvedType::Composite(c) => c,
        other => {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!(
                    "column notation .{field} applied to type {}, which is not a composite type",
                    other.type_name()
                ),
            ));
        }
    };
    match c
        .fields
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(field))
    {
        Some(idx) => {
            let fty = c.fields[idx].1.clone();
            Ok((
                RExpr::Field {
                    base: Box::new(base_node),
                    index: idx,
                },
                fty,
            ))
        }
        None => Err(undefined_column(field)),
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
    // Any subquery makes the enclosing plan un-cacheable: the fold pass rewrites an uncorrelated one
    // (or an uncorrelated one nested inside a correlated one) into a constant using THIS execution's
    // bound params, so a reused plan would carry another execution's folded constants. Every subquery
    // form (scalar / EXISTS / IN / quantified) funnels through here.
    params.uncacheable = true;
    scope
        .catalog
        .plan_query(inner, Some(scope), scope.ctes, params)
}

/// Resolve one array-subscript bound to an integer `RExpr` (a literal adapts to int4; a non-integer
/// is 42804). A NULL-typed bound is accepted — it evaluates to a NULL subscript → NULL result.
fn resolve_subscript_int(
    scope: &Scope,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    let (node, ty) = resolve(scope, e, Some(ScalarType::Int32), agg, params)?;
    if !matches!(ty, ResolvedType::Int(_) | ResolvedType::Null) {
        return Err(type_error(format!(
            "array subscript must be an integer, not {}",
            ty.type_name()
        )));
    }
    Ok(node)
}

fn resolve(
    scope: &Scope,
    e: &Expr,
    ctx: Option<ScalarType>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // GROUP BY a general expression (aggregates.md §15): a non-column expression that structurally
    // matches a grouping-expression key resolves to that group's synthetic key slot — so `SELECT
    // a+b … GROUP BY a+b` projects the grouped value, like a grouping column. Columns keep their own
    // path (matched by index); an aggregate operand / FILTER resolves under `Forbidden`, so this is
    // correctly inert there (its `a+b` is a per-row value, not the group key).
    if !matches!(e, Expr::Column(_) | Expr::QualifiedColumn { .. })
        && let Some((slot, ty)) = match_group_expr(agg, e)
    {
        return Ok((RExpr::Column(slot), ty));
    }
    match e {
        // A `ROW(...)` constructor (spec/design/composite.md §1): resolve each field with no type
        // context (its natural type), producing an ANONYMOUS composite (`name = None`, fields named
        // `f1, f2, …` per PG). Storing it into a named composite column matches structurally
        // (assignability at the store site coerces each field to the target's declared type).
        Expr::Row(items) => {
            let mut nodes = Vec::with_capacity(items.len());
            let mut fields = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                let (node, ty) = resolve(scope, it, None, agg, params)?;
                nodes.push(node);
                fields.push((format!("f{}", i + 1), ty));
            }
            Ok((
                RExpr::Row(nodes),
                ResolvedType::Composite(Box::new(CompositeRType { name: None, fields })),
            ))
        }
        // An `ARRAY[…]` constructor (spec/design/array.md §1): resolve each element (natural type),
        // unify to a common element type, and build a `RExpr::Array`. A bare empty `ARRAY[]` has no
        // element type to infer — use `'{}'::T[]` instead (the cast supplies the element type).
        Expr::Array(items) => {
            if items.is_empty() {
                return Err(type_error(
                    "cannot determine the element type of an empty ARRAY[]; write '{}'::T[]"
                        .to_string(),
                ));
            }
            // An element-type hint (`ctx`) flows down to the elements so an array literal adapts
            // its untyped integer/decimal literals exactly as a scalar literal does — e.g. resolving
            // `ARRAY[7,8]` with an i32 context yields `i32[]`, not the default `i64[]` (the
            // polymorphic array functions pass the bound element type here, array-functions.md §2).
            // Almost every other caller passes `None`, so the default 1-D unification is unchanged.
            let mut nodes = Vec::with_capacity(items.len());
            let mut elem_types = Vec::with_capacity(items.len());
            for it in items {
                let (node, ty) = resolve(scope, it, ctx, agg, params)?;
                nodes.push(node);
                elem_types.push(ty);
            }
            // Unify the item types. If they are themselves arrays, this is a **nested** (multidim-
            // stacking) constructor and the result type is the SAME array type (dimension-agnostic,
            // spec/design/array.md §2/§4); otherwise it is a flat 1-D array of the unified element.
            let common = unify_array_element_types(&elem_types)?;
            let (nested, result_ty) = match common {
                t @ ResolvedType::Array(_) => (true, t),
                other => (false, ResolvedType::Array(Box::new(other))),
            };
            Ok((
                RExpr::Array {
                    elems: nodes,
                    nested,
                },
                result_ty,
            ))
        }
        Expr::Column(name) => {
            // Resolve against the scope CHAIN (§26). Existence first (42703/42702 take priority,
            // matching PostgreSQL); a Local match then obeys the grouping rule, an Outer
            // (correlated) match is a per-outer-row constant exempt from it (see helper).
            let r = scope.resolve_bare(name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        Expr::QualifiedColumn { qualifier, name } => {
            // A bare `rel.col` resolves strictly against the FROM relations — `qualifier` MUST name
            // a relation (else 42P01), matching PostgreSQL. Composite field access on a column is
            // the **parens-required** `(col).field` form (spec/design/composite.md §1/§S4), an
            // `Expr::FieldAccess`, never this bare qualified-column path (PG raises 42P01 for the
            // unparenthesized `col.field` / `t.col.field` spellings).
            let r = scope.resolve_qualified(qualifier, name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        // `(expr).field` — composite field selection (spec/design/composite.md §S4).
        Expr::FieldAccess { base, field } => {
            let (node, ty) = resolve(scope, base, None, agg, params)?;
            resolve_field_of(node, ty, field)
        }
        // `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array
        // (else 42804). Each subscript bound is an integer (PG int4) — a literal adapts; a
        // non-integer is 42804. If any spec is a slice the result is the array type (a sub-array);
        // otherwise it is the element type (a single element). OOB / NULL → NULL is an
        // evaluation-time rule, not a resolve error.
        Expr::Subscript { base, subscripts } => {
            let (base_node, base_ty) = resolve(scope, base, None, agg, params)?;
            let elem_ty = match &base_ty {
                ResolvedType::Array(elem) => (**elem).clone(),
                other => {
                    return Err(type_error(format!(
                        "cannot subscript a value of type {}, which is not an array",
                        other.type_name()
                    )));
                }
            };
            let is_slice = subscripts
                .iter()
                .any(|s| matches!(s, SubscriptSpec::Slice(..)));
            let mut rsubs = Vec::with_capacity(subscripts.len());
            for s in subscripts {
                match s {
                    SubscriptSpec::Index(e) => {
                        rsubs.push(RSubscript::Index(Box::new(resolve_subscript_int(
                            scope, e, agg, params,
                        )?)));
                    }
                    SubscriptSpec::Slice(lo, hi) => {
                        let lower = match lo {
                            Some(e) => {
                                Some(Box::new(resolve_subscript_int(scope, e, agg, params)?))
                            }
                            None => None,
                        };
                        let upper = match hi {
                            Some(e) => {
                                Some(Box::new(resolve_subscript_int(scope, e, agg, params)?))
                            }
                            None => None,
                        };
                        rsubs.push(RSubscript::Slice { lower, upper });
                    }
                }
            }
            // A slice yields a sub-array (the array type); all-index access yields an element.
            let result_ty = if is_slice { base_ty } else { elem_ty };
            Ok((
                RExpr::Subscript {
                    base: Box::new(base_node),
                    subscripts: rsubs,
                    is_slice,
                },
                result_ty,
            ))
        }
        // `(expr).*` — whole-row expansion is a projection-list construct only; in a scalar
        // expression position it is unsupported (PG rejects row expansion here — 0A000).
        Expr::FieldStar { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "row expansion (.*) is not supported in this context",
        )),
        // `t.*` is likewise projection-list only — resolve_projections expands it before ever
        // calling resolve(); reaching here means it appeared in a scalar position (`WHERE t.*`,
        // `t.* + 1`), which is a syntax error (PG rejects a bare `t.*` outside the select list).
        Expr::QualifiedStar { .. } => Err(EngineError::new(
            SqlState::SyntaxError,
            "t.* is only allowed in a select list",
        )),
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
        Expr::FuncCall {
            name,
            args,
            arg_names,
            star,
            distinct,
            filter,
            variadic,
            over,
            over_name: _, // desugared to `over` before resolution (window.md §5)
            within_group,
        } => {
            // A hypothetical-set aggregate (rank/dense_rank/percent_rank/cume_dist — aggregates.md
            // §19) is one of these window-function names used WITH a WITHIN GROUP clause; that clause
            // routes it here instead of the window path. OVER + WITHIN GROUP together is 0A000.
            if is_hypothetical_set_name(name)
                && let Some(keys) = within_group.as_deref()
            {
                if over.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "OVER is not supported for hypothetical-set aggregate {}",
                            name.to_ascii_lowercase()
                        ),
                    ));
                }
                return resolve_hypothetical_set_aggregate(
                    scope,
                    name,
                    args,
                    keys,
                    *distinct,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // An ordered-set aggregate (mode/percentile_cont/percentile_disc — aggregates.md §13)
            // carries WITHIN GROUP and is resolved by its own path. OVER on one is 0A000 (PG itself
            // does not support an ordered-set aggregate as a window function); WITHOUT a WITHIN GROUP
            // it is 42883 (PG: "function mode() does not exist").
            if is_ordered_set_aggregate_name(name) {
                if over.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "OVER is not supported for ordered-set aggregate {}",
                            name.to_ascii_lowercase()
                        ),
                    ));
                }
                let Some(keys) = within_group.as_deref() else {
                    return Err(no_agg_overload(&name.to_ascii_lowercase()));
                };
                return resolve_ordered_set_aggregate(
                    scope,
                    name,
                    args,
                    keys,
                    *distinct,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // WITHIN GROUP on a non-ordered-set function (an ordinary aggregate or a scalar function)
            // is 42883 — PG models it as a missing overload (`sum(numeric, numeric) does not exist`).
            if within_group.is_some() {
                return Err(no_agg_overload(&name.to_ascii_lowercase()));
            }
            // A trailing OVER makes this a window-function call (spec/design/window.md §5.1).
            if let Some(wd) = over {
                // GROUPING is not a window function — `GROUPING(a) OVER ()` is a syntax error in
                // PostgreSQL (42601); match it rather than treating GROUPING as an unknown window fn.
                if name.eq_ignore_ascii_case("grouping") {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "OVER is not supported for GROUPING",
                    ));
                }
                // DISTINCT is not implemented for window functions (PG 0A000 — aggregates.md §5):
                // a window aggregate folds over a frame, where per-frame de-duplication is undefined.
                if *distinct {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "DISTINCT is not implemented for window functions",
                    ));
                }
                // FILTER over a window function (aggregates.md §20). A window AGGREGATE folds only the
                // frame rows for which the filter is TRUE; a pure (non-aggregate) window function with
                // FILTER is PG's own 0A000 ("FILTER is not implemented for non-aggregate window
                // functions"). The filter is threaded into the WindowSpec and applied in the window stage.
                if filter.is_some() && !is_aggregate_name(name) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "FILTER is not implemented for non-aggregate window functions",
                    ));
                }
                return resolve_window_call(
                    scope,
                    name,
                    args,
                    *star,
                    wd,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // A window-only function (row_number/…) used WITHOUT OVER is 42809 (PG's
            // wrong_object_type, not the windowing_error 42P20 it uses for a window in WHERE —
            // window.md §7, oracle-verified).
            if is_window_only_name(name) {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!(
                        "window function {} requires an OVER clause",
                        name.to_ascii_lowercase()
                    ),
                ));
            }
            let names = arg_names.as_deref().map(Vec::as_slice);
            resolve_func_call(
                scope,
                name,
                args,
                names,
                *star,
                *distinct,
                filter.as_deref(),
                *variadic,
                agg,
                params,
            )
        }
        Expr::Literal(Literal::Null) => Ok((RExpr::ConstNull, ResolvedType::Null)),
        Expr::Literal(Literal::Bool(b)) => Ok((RExpr::ConstBool(*b), ResolvedType::Bool)),
        Expr::Literal(Literal::Int(n)) => {
            // An integer literal ADAPTS to a float context — decimal/int literal → float at the
            // context width (nearest, round-ties-to-even — spec/design/float.md §4). This is
            // literal adaptation, not an implicit cross-family cast (a *value* never silently
            // becomes a float). Otherwise it adapts only to an integer context, defaulting to
            // i64; a non-numeric context defers the family-mismatch check to the surroundings.
            if let Some(t) = ctx.filter(|t| t.is_float()) {
                return Ok((int_to_const_float(*n, t), ResolvedType::Float(t)));
            }
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
                // A string adapts to a DATE context (parse the ISO date, dropping any time/offset;
                // 22007/22008 — spec/design/date.md §2), exactly like timestamp adaptation.
                Some(t) if t.is_date() => {
                    Ok((RExpr::ConstDate(parse_date(s)?), ResolvedType::Date))
                }
                // A string adapts to an INTERVAL context (parse the "unit + time" subset,
                // 22007/22008 — spec/design/interval.md), exactly like timestamp adaptation.
                Some(t) if t.is_interval() => Ok((
                    RExpr::ConstInterval(parse_interval(s)?),
                    ResolvedType::Interval,
                )),
                // A string literal adapts to a json/jsonb context (the sibling of a jsonb column /
                // a jsonb cast), so `jsonbcol = '{"a":1}'` compares jsonb × jsonb; malformed → 22P02
                // (spec/design/json.md §2/§4). json validates + stores verbatim; jsonb canonicalizes.
                Some(t) if t.is_json() => {
                    json::validate_json(s)?;
                    Ok((RExpr::ConstJson(s.clone()), ResolvedType::Json))
                }
                Some(t) if t.is_jsonb() => Ok((
                    RExpr::ConstJsonb(Box::new(json::jsonb_in(s)?)),
                    ResolvedType::Jsonb,
                )),
                // A string literal adapts to a jsonpath context (a jsonpath function argument) — it is
                // compiled to a path at resolve (jsonpath.md §1); malformed → 42601.
                Some(ScalarType::JsonPath) => Ok((
                    RExpr::ConstJsonPath(crate::jsonpath::JsonPath::compile(s)?.render()),
                    ResolvedType::JsonPath,
                )),
                _ => Ok((RExpr::ConstText(s.clone()), ResolvedType::Text)),
            }
        }
        Expr::Literal(Literal::Decimal(d)) => {
            // A decimal literal ADAPTS to a float context — decimal → float at the context width
            // (nearest binary value, round-ties-to-even — spec/design/float.md §4). Otherwise it
            // stays decimal (it does not adapt to other contexts, like text). Cap-check the
            // decimal value here (an over-long coefficient/scale traps 22003 at resolve —
            // spec/design/decimal.md §6).
            if let Some(t) = ctx.filter(|t| t.is_float()) {
                return Ok(match decimal_to_float(d, t)? {
                    Value::Float32(f) => (RExpr::ConstFloat32(f), ResolvedType::Float(t)),
                    Value::Float64(f) => (RExpr::ConstFloat64(f), ResolvedType::Float(t)),
                    _ => unreachable!("decimal_to_float returns a float value"),
                });
            }
            let d = d.clone().check_cap()?;
            Ok((RExpr::ConstDecimal(d), ResolvedType::Decimal))
        }
        // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
        // `type 'string'`, equal to `CAST('string' AS type)` over a string-literal operand. Resolve
        // the type by name (unknown → 42704) and coerce the string to it at resolve, independent of
        // any context. No typmod rides on the literal (the parser's one-token lookahead admits none).
        Expr::TypedLiteral { type_name, text } => {
            // A composite type name (`addr '(Main,90210)'`) coerces the string via `record_in`
            // (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
            if let Some(ct) = scope.catalog.composite_type(type_name) {
                return coerce_string_to_composite(text, ct, scope.catalog);
            }
            // A range type name (`i32range '[1,5)'`, `int4range '…'`) coerces the string via
            // `range_in` against the element type (spec/design/ranges.md §5) — the same primitive
            // as `'[1,5)'::i32range`.
            if let Some(desc) = crate::range::range_by_name(type_name) {
                return coerce_string_to_range_expr(text, desc);
            }
            let (target, _, _) = resolve_type_and_typmod(type_name, &None)?;
            coerce_string_literal(text, target, None, None)
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
            let out_type = plan.column_types()[0].clone();
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
            classify_comparable(&lt, &plan.column_types()[0])?;
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
        // `expr COLLATE "name"` (spec/design/collation.md §1) — a postfix collation operator. Resolve
        // the inner expression, require a collatable (text) type (42804, PG-matching), and validate
        // the named collation exists ("C" or loaded, else 42704). The node is a runtime PASSTHROUGH:
        // a collation only changes the ORDERING comparisons / ORDER BY, derived from the AST at those
        // sites (`explicit_collation` / `OrderKey.collation`), so resolving returns the inner resolved
        // expr + type unchanged. The hint flows through (COLLATE never changes the type).
        Expr::Collate { inner, collation } => {
            let (rinner, ty) = resolve(scope, inner, ctx, agg, params)?;
            if !matches!(ty, ResolvedType::Text | ResolvedType::Null) {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    ty.type_name()
                )));
            }
            // Validate the name resolves (surfaces 42704 for an unknown collation); the value is
            // recovered at the comparison/ORDER BY site, so it is discarded here.
            resolve_collation_name(scope.catalog, collation)?;
            Ok((rinner, ty))
        }
        // `EXTRACT(field FROM source)` (timezones.md §9.2, grammar.md §50). The field is SYNTACTIC and
        // validated at RESOLVE (not per row): an unsupported field for the source type is `0A000`, an
        // unrecognized field is `22023` — surfaced by probing the kernel with a zero value of the
        // source's family. The source must be a datetime type (else `42883`); the result is `numeric`.
        Expr::Extract { field, source } => {
            use crate::datetime_fn::{ExtractSrc, extract_field};
            let (src_r, src_t) = resolve(scope, source, None, agg, params)?;
            // A NULL source has no resolvable family; the value propagates to NULL at eval (the field
            // is not validated — a documented narrow edge vs. PG, which still errors on a bad field).
            if !matches!(src_t, ResolvedType::Null) {
                let probe = match src_t {
                    ResolvedType::Timestamp => ExtractSrc::Timestamp(0),
                    ResolvedType::Timestamptz => ExtractSrc::Timestamptz {
                        instant: 0,
                        local: 0,
                        offset_secs: 0,
                    },
                    ResolvedType::Date => ExtractSrc::Date(0),
                    ResolvedType::Interval => ExtractSrc::Interval(crate::interval::Interval {
                        months: 0,
                        days: 0,
                        micros: 0,
                    }),
                    _ => {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            format!(
                                "function extract(text, {}) does not exist",
                                src_t.type_name()
                            ),
                        ));
                    }
                };
                // Validate field-for-type (0A000 / 22023); the value is discarded.
                extract_field(field, probe)?;
            }
            Ok((
                RExpr::Extract {
                    field: field.clone(),
                    value: Box::new(src_r),
                },
                ResolvedType::Decimal,
            ))
        }
        Expr::Cast {
            inner,
            type_name,
            type_mod,
        } => {
            // An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the
            // string-literal form `'{…}'::T[]` and a bare NULL; every other array cast (runtime
            // text→array, array→text, element-wise array→array) is a documented 0A000 narrowing.
            // The element is a scalar or a previously-defined composite (array-of-composite, §12 AC1).
            if let Some(base) = type_name.strip_suffix("[]") {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on an array type is not supported yet".to_string(),
                    ));
                }
                let (elem_col, elem_rt): (ColType, ResolvedType) = match ScalarType::from_name(base)
                {
                    Some(s) => (ColType::Scalar(s), resolved_type_of(s)),
                    None => match scope.catalog.composite_type(base) {
                        Some(ct) => {
                            let cty = Type::Composite(crate::types::CompositeRef {
                                name: ct.name.clone(),
                            });
                            let col = resolve_col_type(&cty, &scope.catalog.read_snap().types);
                            let rt = resolved_type_of_col(&cty, scope.catalog);
                            (col, rt)
                        }
                        None => {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("type does not exist: {base}"),
                            ));
                        }
                    },
                };
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    let val = coerce_string_to_array(s, &elem_col)?;
                    return Ok((value_to_rexpr(&val), ResolvedType::Array(Box::new(elem_rt))));
                }
                if let Expr::Literal(Literal::Null) = inner.as_ref() {
                    return Ok((RExpr::ConstNull, ResolvedType::Array(Box::new(elem_rt))));
                }
                // A bind parameter into an array stays the documented container-param narrowing
                // (0A000), like INSERT's `$N`-into-a-container handling (spec/design/array.md §4).
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a parameter to an array type is not supported yet".to_string(),
                    ));
                }
                // A runtime (non-literal) operand: the two follow-on array-producing casts
                // (spec/design/array.md §7). A `text` expression coerces per row via `array_in`
                // (runtime text→T[]); an array of the SAME element type is the identity (no node);
                // an array of a DIFFERENT element type is an element-wise array→array cast (each
                // element through the scalar cast, when the element pair is castable); a non-literal
                // NULL adapts. Any other source is a 42804 datatype mismatch.
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let result_rt = ResolvedType::Array(Box::new(elem_rt.clone()));
                return match ity {
                    ResolvedType::Null => Ok((rinner, result_rt)),
                    ResolvedType::Text => Ok((
                        RExpr::ArrayCast {
                            inner: Box::new(rinner),
                            to_elem: Some(elem_col),
                        },
                        result_rt,
                    )),
                    ResolvedType::Array(ref src_elem) if **src_elem == elem_rt => {
                        Ok((rinner, result_rt)) // identity cast — same element type
                    }
                    ResolvedType::Array(ref src_elem) => {
                        match (resolved_to_scalar(src_elem), &elem_col) {
                            (Some(src_s), ColType::Scalar(tgt_s))
                                if scalar_pair_castable(src_s, *tgt_s) =>
                            {
                                Ok((
                                    RExpr::ArrayCast {
                                        inner: Box::new(rinner),
                                        to_elem: Some(elem_col),
                                    },
                                    result_rt,
                                ))
                            }
                            // A composite element on either side is the composite cast surface
                            // (0A000 — composite casts are deferred, composite.md §8/§12).
                            (None, _) | (_, ColType::Composite { .. }) => Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                "casting between composite-element arrays is not supported yet"
                                    .to_string(),
                            )),
                            // Both elements are scalars but no cast exists between them — forbidden
                            // (42804, jed's strict-matrix convention; PG reports 42846).
                            _ => Err(type_error(format!(
                                "cannot cast {} to {base}[]",
                                ity.type_name()
                            ))),
                        }
                    }
                    _ => Err(type_error(format!(
                        "cannot cast {} to {base}[]",
                        ity.type_name()
                    ))),
                };
            }
            // A range cast target (`'[1,5)'::i32range`, `…::int4range`). Like array, v1 supports the
            // string-literal form and a bare NULL; every other range cast (runtime text→range,
            // range→text) is a documented 0A000 narrowing (spec/design/ranges.md §1/§5).
            if let Some(desc) = crate::range::range_by_name(type_name) {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on a range type is not supported".to_string(),
                    ));
                }
                let elem_rt = resolved_type_of(crate::range::element_scalar(desc));
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    return coerce_string_to_range_expr(s, desc);
                }
                if let Expr::Literal(Literal::Null) = inner.as_ref() {
                    return Ok((RExpr::ConstNull, ResolvedType::Range(Box::new(elem_rt))));
                }
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to a range type is only supported from a string literal this slice"
                        .to_string(),
                ));
            }
            // A composite cast target (`'(…)'::addr`) — a CREATE TYPE name, not a built-in scalar
            // (spec/design/composite.md §8). A STRING LITERAL operand coerces via `record_in` (the
            // `'(…)'::addr` headline); a bare NULL adapts to the composite; a same-named composite
            // operand is the identity. Every other operand (a runtime text expression, an anonymous
            // `ROW(…)`) is a documented `0A000` narrowing this slice — relaxable. A type modifier on
            // a composite is meaningless (`0A000`).
            if let Some(ct) = scope.catalog.composite_type(type_name) {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier is not supported on a composite type",
                    ));
                }
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    return coerce_string_to_composite(s, ct, scope.catalog);
                }
                let ct_name = ct.name.clone();
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match &ity {
                    ResolvedType::Null => Ok((
                        rinner,
                        resolved_type_of_col(
                            &Type::Composite(crate::types::CompositeRef { name: ct_name }),
                            scope.catalog,
                        ),
                    )),
                    // An identical named composite is the identity cast.
                    ResolvedType::Composite(c) if c.name.as_deref() == Some(ct_name.as_str()) => {
                        Ok((rinner, ity))
                    }
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to a composite type is only supported from a string literal",
                    )),
                };
            }
            let (target, typmod, varchar_len) = resolve_type_and_typmod(type_name, type_mod)?;
            // A string LITERAL operand is coerced to the target at resolve — `CAST('42' AS int)`,
            // the same primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5).
            // This is the ONLY text→T cast admitted ahead of the general cast slice; a non-literal
            // text operand still falls through to the deferred 0A000 below. A `varchar(n)` target
            // truncates the literal to n code points (types.md §15).
            if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                return coerce_string_literal(s, target, typmod, varchar_len);
            }
            // Cross-family datetime casts (timezones.md §9.3): a `timestamp`/`timestamptz`/`date`
            // TARGET from another datetime family. A same-family cast is the identity; a cross-family
            // cast becomes a `DateConvert` node (the zone-crossing ones read the session zone at eval);
            // any non-datetime source is the deferred `0A000`. `text`-literal operands and bind params
            // are handled above / just below. A `NULL` operand adapts to the target.
            if target.is_timestamp() || target.is_timestamptz() || target.is_date() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    // `$1::timestamp` declares the parameter as the target type (the cast-target
                    // parameter-typing case, api.md §5), exactly like the generic path below.
                    let (rinner, _) = resolve(scope, inner, Some(target), agg, params)?;
                    return Ok((rinner, resolved_type_of(target)));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let to_rt = resolved_type_of(target);
                return match ity {
                    ResolvedType::Null => Ok((rinner, to_rt)),
                    ResolvedType::Timestamp if target.is_timestamp() => Ok((rinner, ity)),
                    ResolvedType::Timestamptz if target.is_timestamptz() => Ok((rinner, ity)),
                    ResolvedType::Date if target.is_date() => Ok((rinner, ity)),
                    ResolvedType::Timestamp | ResolvedType::Timestamptz | ResolvedType::Date => {
                        Ok((
                            RExpr::DateConvert {
                                inner: Box::new(rinner),
                                to: target,
                            },
                            to_rt,
                        ))
                    }
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "cannot cast {} to {}",
                            ity.type_name(),
                            target.canonical_name()
                        ),
                    )),
                };
            }
            // The JSON cast matrix (spec/design/json.md §6.1): casting TO json/jsonb from a runtime
            // text/json/jsonb expression (a string LITERAL operand was already coerced above by
            // `coerce_string_literal`). text → json validates + stores verbatim; text → jsonb parses
            // + canonicalizes; json → jsonb re-parses + canonicalizes; jsonb → json renders the
            // canonical text; same-type is the identity. Any other source is a 42846 cast error.
            if target.is_json() || target.is_jsonb() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(target), agg, params)?;
                    return Ok((rinner, resolved_type_of(target)));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let to_rt = resolved_type_of(target);
                return match ity {
                    ResolvedType::Null => Ok((rinner, to_rt)),
                    ResolvedType::Text | ResolvedType::Json | ResolvedType::Jsonb => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        to_rt,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast type {} to {}",
                        ity.type_name(),
                        target.canonical_name()
                    ))),
                };
            }
            // Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11), EXCEPT
            // json/jsonb → text (the JSON cast matrix, json.md §6.1): json → text is the identity on
            // the verbatim bytes, jsonb → text renders the canonical form. A NULL adapts. Every other
            // text cast target is still a 0A000 this slice — including `$1::text` (declaring a bind
            // param as text via a cast stays deferred, the params.rs contract — guarded first so it
            // does not resolve to an untyped-NULL text node and trip 42P18).
            if target.is_text() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to text is not supported yet",
                    ));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    // text → text: the identity, UNLESS a `varchar(n)` length is present — then it
                    // becomes a real Cast node that silently truncates to n code points at eval
                    // (types.md §15). A NULL adapts (NULL → NULL, no truncation needed).
                    ResolvedType::Null => Ok((rinner, ResolvedType::Text)),
                    ResolvedType::Text => Ok((
                        match varchar_len {
                            Some(_) => RExpr::Cast {
                                inner: Box::new(rinner),
                                target,
                                typmod: None,
                                varchar_len,
                            },
                            None => rinner,
                        },
                        ResolvedType::Text,
                    )),
                    // json/jsonb → text (the JSON cast matrix) and uuid → text (the uuid cast slice,
                    // casts.toml/types.md §14: the canonical lowercase 8-4-4-4-12 form). Explicit —
                    // stricter than PG's assignment-cast-to-text (a documented divergence). A
                    // `varchar(n)` length truncates the rendered text (types.md §15).
                    ResolvedType::Json | ResolvedType::Jsonb | ResolvedType::Uuid => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len,
                        },
                        ResolvedType::Text,
                    )),
                    // array → text (spec/design/array.md §7): `array_out` renders `{…}` per row.
                    // Explicit only — like uuid/json → text, stricter than PG's assignment cast (so
                    // `INSERT INTO text_col VALUES (arr)` stays 42804). Handled by `ArrayCast`.
                    ResolvedType::Array(_) => Ok((
                        RExpr::ArrayCast {
                            inner: Box::new(rinner),
                            to_elem: None,
                        },
                        ResolvedType::Text,
                    )),
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to text is not supported yet",
                    )),
                };
            }
            // A boolean target (`CAST(x AS boolean)`, `x::boolean`) is the boolean cast slice
            // (spec/types/casts.toml, types.md §9). It needs the inner type to decide (only an i32
            // / NULL / bool source is castable), so it is handled AFTER the inner is resolved, below
            // — not guarded here.
            // A bytea TARGET: the uuid cast slice admits uuid → bytea (the 16 raw bytes — a jed cast
            // PG lacks; casts.toml, types.md §14). A string LITERAL was coerced above; a NULL adapts;
            // a bytea operand is the identity. text → bytea and every other bytea cast stay deferred
            // (0A000 — the bytea cast slice's own follow-on, types.md §13).
            if target.is_bytea() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(ScalarType::Bytea), agg, params)?;
                    return Ok((rinner, ResolvedType::Bytea));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    ResolvedType::Null | ResolvedType::Bytea => Ok((rinner, ResolvedType::Bytea)),
                    ResolvedType::Uuid => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        ResolvedType::Bytea,
                    )),
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to bytea is not supported yet",
                    )),
                };
            }
            // The uuid cast slice (spec/types/casts.toml, types.md §14): a uuid TARGET from a runtime
            // text or bytea expression. text → uuid runs uuid_in at eval (22P02 on malformed);
            // bytea → uuid takes the 16 raw bytes (22P02 on a length ≠ 16) — a jed cast PG lacks. A
            // string LITERAL operand was already coerced above (the §6 adaptation); `$1::uuid`
            // declares the param as uuid; a NULL adapts; a uuid operand is the identity.
            if target.is_uuid() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(ScalarType::Uuid), agg, params)?;
                    return Ok((rinner, ResolvedType::Uuid));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    ResolvedType::Null | ResolvedType::Uuid => Ok((rinner, ResolvedType::Uuid)),
                    ResolvedType::Text | ResolvedType::Bytea => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        ResolvedType::Uuid,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast {} to uuid",
                        ity.type_name()
                    ))),
                };
            }
            // The timestamp/timestamptz/date cross-family cast matrix is handled above (the
            // `DateConvert` block — timezones.md §9.3). `text`↔datetime casts (a string lands in a
            // datetime column by literal adaptation, not a CAST) stay deferred and fall through to the
            // generic logic below (which rejects a non-datetime source to a datetime target 0A000).
            // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000
            // (a string lands in an interval column by literal adaptation / the INTERVAL '...'
            // keyword literal, not a CAST).
            if target.is_interval() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to an interval type is not supported yet",
                ));
            }
            // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int`
            // (and `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
            // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
            // context — its value is range-checked / coerced against `target` at eval — so changing
            // the context only for a parameter leaves all existing CAST behavior untouched.
            let inner_ctx = if matches!(inner.as_ref(), Expr::Param(_)) {
                Some(target)
            } else if target.is_bool() {
                // A boolean TARGET accepts only an i32 source (the boolean cast slice). An untyped
                // integer literal operand therefore adapts to i32 — `CAST(5 AS boolean)` / `5::boolean`
                // — matching PG (a bare `5` is int4, then int4→bool). Without this the literal would
                // default to i64 and the i64→boolean pair is forbidden. A column/expression operand
                // ignores this literal context and keeps its own type (an i64 column → 42804). A
                // literal beyond i32 range then traps 22003 (PG says 42846 — a documented divergence).
                Some(ScalarType::Int32)
            } else {
                None
            };
            let (rinner, ity) = resolve(scope, inner, inner_ctx, agg, params)?;
            // The boolean cast slice (spec/types/casts.toml, types.md §9): PG ties boolean↔integer to
            // i32 ONLY and makes both directions explicit. A boolean TARGET takes an i32 / NULL / bool
            // source (the eval maps 0→false, nonzero→true); a boolean SOURCE produces an i32 (true→1,
            // false→0). Both are handled here, ahead of the generic numeric cast logic below — the
            // generic `result_ty` assumes an int/decimal/float target, so a boolean target must not
            // fall through. A bool⇄i16 / bool⇄i64 pair is a forbidden 42804 (jed's datatype-mismatch
            // convention; PG reports 42846 — a documented divergence, casts.toml).
            if target.is_bool() {
                return match ity {
                    // A runtime `text` source is the runtime-text-cast slice (grammar.md §36): the
                    // eval parses the per-row string via the same `parse_bool_literal` (PG boolin)
                    // the `'t'::boolean` literal uses. A string LITERAL operand was already coerced
                    // above, so a `Text` here is non-literal (a column / expression).
                    ResolvedType::Int(ScalarType::Int32)
                    | ResolvedType::Bool
                    | ResolvedType::Text
                    | ResolvedType::Null => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod,
                            varchar_len: None,
                        },
                        ResolvedType::Bool,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast {} to boolean",
                        ity.type_name()
                    ))),
                };
            }
            if matches!(ity, ResolvedType::Bool) {
                if target == ScalarType::Int32 {
                    return Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod,
                            varchar_len: None,
                        },
                        ResolvedType::Int(ScalarType::Int32),
                    ));
                }
                return Err(type_error(format!(
                    "cannot cast boolean to {}",
                    target.canonical_name()
                )));
            }
            match ity {
                // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
                // decimal→decimal (re-scale), and NULL are all castable. Floats add int↔float,
                // decimal↔float, and float↔float (spec/design/float.md §6 — all explicit; the
                // eval does the rounding/range-check), so a Float inner is castable too.
                ResolvedType::Int(_)
                | ResolvedType::Decimal
                | ResolvedType::Float(_)
                | ResolvedType::Null => {}
                // A boolean source is handled above (the boolean cast slice) — unreachable here.
                ResolvedType::Bool => unreachable!("boolean cast operand handled above"),
                // A runtime `text` source to a numeric target is the runtime-text-cast slice
                // (grammar.md §36): the only targets reaching this generic path are int / decimal /
                // float (text / bytea / uuid / datetime / interval / bool / json targets all return
                // in their own blocks above), so a `Text` here casts to a number. The eval coerces
                // the per-row string via the same parse functions the literal form uses (22P02 /
                // 22003 per row). A string LITERAL operand was already folded above, so this `Text`
                // is non-literal (a column / expression). Fall through to the numeric `Cast` node.
                ResolvedType::Text => {}
                // Casting FROM bytea is likewise deferred (0A000).
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
                // Casting FROM an interval is likewise deferred (0A000).
                ResolvedType::Interval => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from an interval type is not supported yet",
                    ));
                }
                // Casting FROM a date is likewise deferred (0A000; date↔timestamp unblocks the
                // cross-family comparison — spec/design/date.md §4/§6).
                ResolvedType::Date => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from a date type is not supported yet",
                    ));
                }
                // Casting a composite (text↔composite) lands in a later slice (composite.md §8/§12).
                ResolvedType::Composite(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a composite value is not supported yet",
                    ));
                }
                // Casting FROM an array (array→text, element-wise array→array) is deferred
                // (spec/design/array.md §7/§12).
                ResolvedType::Array(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting an array value is not supported yet",
                    ));
                }
                // Casting FROM a range (range→text, range→range) is deferred (ranges.md §5/§10);
                // a range cast TARGET is handled above (the string-literal form).
                ResolvedType::Range(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a range value is not supported yet",
                    ));
                }
                // Casting FROM json/jsonb (json↔jsonb, json[b]→text, text→json[b]) lands in J3
                // (spec/design/json.md §6); deferred this slice.
                ResolvedType::Json | ResolvedType::Jsonb | ResolvedType::JsonPath => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a json value is not supported yet",
                    ));
                }
            }
            let result_ty = if target.is_decimal() {
                ResolvedType::Decimal
            } else if target.is_float() {
                ResolvedType::Float(target)
            } else {
                ResolvedType::Int(target)
            };
            Ok((
                RExpr::Cast {
                    inner: Box::new(rinner),
                    target,
                    typmod,
                    varchar_len: None,
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
                // -float flips the sign bit (no overflow; a NaN/Inf operand passes through —
                // spec/design/float.md §5). The result keeps the operand's width.
                ResolvedType::Float(t) => t,
                ResolvedType::Null => ScalarType::Int64, // -NULL = NULL
                ResolvedType::Interval => ScalarType::Interval, // -interval (interval.md §5)
                ResolvedType::Bool
                | ResolvedType::Text
                | ResolvedType::Bytea
                | ResolvedType::Uuid
                | ResolvedType::Timestamp
                | ResolvedType::Timestamptz
                | ResolvedType::Date
                | ResolvedType::Json
                | ResolvedType::Jsonb
                | ResolvedType::JsonPath
                | ResolvedType::Composite(_)
                | ResolvedType::Array(_)
                | ResolvedType::Range(_) => {
                    return Err(type_error("unary minus requires a numeric operand"));
                }
            };
            let rty = if result.is_decimal() {
                ResolvedType::Decimal
            } else if result.is_interval() {
                ResolvedType::Interval
            } else if result.is_float() {
                ResolvedType::Float(result)
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
            require_bool(&ty, "NOT requires a boolean operand")?;
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
        Expr::IsJson {
            operand,
            negated,
            kind,
            unique_keys,
        } => {
            // The operand must be a character string / json / jsonb (else 42804); a bare string
            // literal resolves as text. The predicate is always a definite boolean (NULL operand →
            // NULL at eval).
            let (rop, ty) = resolve(scope, operand, None, agg, params)?;
            match ty {
                ResolvedType::Text
                | ResolvedType::Json
                | ResolvedType::Jsonb
                | ResolvedType::JsonPath
                | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!("cannot use type {} in IS JSON predicate", ty.type_name()),
                    ));
                }
            }
            Ok((
                RExpr::IsJson {
                    operand: Box::new(rop),
                    negated: *negated,
                    kind: *kind,
                    unique_keys: *unique_keys,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::JsonCtor {
            operand,
            unique_keys,
        } => {
            // JSON(text) parses a character string to a `json` value (verbatim). The operand must be
            // text (a bare string literal stays text); a non-text operand → 42804.
            let (rop, ty) = resolve(scope, operand, Some(ScalarType::Text), agg, params)?;
            match ty {
                ResolvedType::Text | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!("cannot use type {} as JSON() input", ty.type_name()),
                    ));
                }
            }
            Ok((
                RExpr::JsonCtor {
                    operand: Box::new(rop),
                    unique_keys: *unique_keys,
                },
                ResolvedType::Json,
            ))
        }
        Expr::JsonExists {
            ctx,
            path,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Exists,
            ctx,
            path,
            &None,
            JsonWrapper::Without,
            true,
            &None,
            on_error,
            agg,
            params,
        ),
        Expr::JsonValue {
            ctx,
            path,
            returning,
            on_empty,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Value,
            ctx,
            path,
            returning,
            JsonWrapper::Without,
            true,
            on_empty,
            on_error,
            agg,
            params,
        ),
        Expr::JsonQuery {
            ctx,
            path,
            returning,
            wrapper,
            keep_quotes,
            on_empty,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Query,
            ctx,
            path,
            returning,
            *wrapper,
            *keep_quotes,
            on_empty,
            on_error,
            agg,
            params,
        ),
        Expr::IsDistinctFrom { lhs, rhs, negated } => {
            // NULL-safe equality: the SAME operand contract as `=` — resolve the pair
            // (a literal adapts to its sibling; a text literal stays text), then require
            // the operands be comparable (both integer-ish or both text-ish; a mixed pair
            // is 42804). The result is always a definite boolean (functions.md §3).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(&lt, &rt)?;
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
        Expr::Quantified {
            op,
            all,
            lhs,
            array,
        } => resolve_quantified(scope, *op, *all, lhs, array, agg, params),
        Expr::QuantifiedSubquery {
            op,
            all,
            lhs,
            query,
        } => {
            // The subquery spelling of the quantifier (array-functions.md §11.6) — the IN-subquery
            // pattern, with the comparison + 3VL fold of the array form. Resolve the outer `lhs`,
            // plan the body, require ONE column (42601), and require comparability — reporting
            // operator-not-found (42883) the way the array quantifier does (§11.3), not the plain
            // 42804. No 21000 cardinality limit (any row count is a list).
            let (rlhs, lt) = resolve(scope, lhs, None, agg, params)?;
            let plan = plan_subquery(scope, query, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery has too many columns",
                ));
            }
            classify_comparable(&lt, &plan.column_types()[0]).map_err(|_| {
                EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: {} {} {}",
                        lt.type_name(),
                        binary_op_symbol(*op),
                        plan.column_types()[0].type_name()
                    ),
                )
            })?;
            let cop = match op {
                BinaryOp::Eq => CmpOp::Eq,
                BinaryOp::Ne => CmpOp::Ne,
                BinaryOp::Lt => CmpOp::Lt,
                BinaryOp::Gt => CmpOp::Gt,
                BinaryOp::Le => CmpOp::Le,
                BinaryOp::Ge => CmpOp::Ge,
                _ => unreachable!(
                    "the parser only builds a quantified node for a comparison operator"
                ),
            };
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Quantified { op: cop, all: *all },
                    lhs: Some(Box::new(rlhs)),
                    negated: false,
                },
                ResolvedType::Bool,
            ))
        }
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
        Expr::Like {
            lhs,
            rhs,
            negated,
            insensitive,
        } => {
            // LIKE / ILIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
            // stays text), then require BOTH operands be text (or a bare NULL); a non-text
            // operand is 42804. We do NOT use classify_comparable here — it would wrongly accept
            // bytea×bytea, which LIKE does not define.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_text_or_null(&lt)?;
            require_text_or_null(&rt)?;
            Ok((
                RExpr::Like {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                    insensitive: *insensitive,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Regex {
            lhs,
            rhs,
            negated,
            insensitive,
        } => {
            // ~ / ~* / !~ / !~* — text×text → boolean (grammar.md §22b, regex.md). Same operand
            // typing as LIKE: resolve the pair, require both text (or a bare NULL); non-text 42804.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_text_or_null(&lt)?;
            require_text_or_null(&rt)?;
            // Precompile a CONSTANT pattern ONCE (regex.md §5); a non-constant pattern compiles per
            // row at eval. For ~* the constant is case-folded before compiling (the ILIKE
            // mechanism); the subject is folded per row at eval. A malformed pattern surfaces 2201B
            // (and an oversized one 54001) here, at resolve, for the constant case.
            let program = if let RExpr::ConstText(pat) = &rr {
                let folded;
                let pat_ref = if *insensitive {
                    let prop = crate::collation::loaded_property();
                    folded = crate::collation::fold_lower_simple(pat, prop.as_deref());
                    folded.as_str()
                } else {
                    pat.as_str()
                };
                Some(crate::regex::compile(pat_ref)?)
            } else {
                None
            };
            // A precompiled program carries the one-shot `compile_charged` cost flag mutated on first
            // eval, so a reused plan would under-charge the 2nd+ execute — never cache such a plan.
            if program.is_some() {
                params.uncacheable = true;
            }
            Ok((
                RExpr::Regex {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                    insensitive: *insensitive,
                    program,
                    compile_charged: std::cell::Cell::new(false),
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
                        require_bool(&cty, "CASE WHEN condition must be boolean")?;
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

/// Resolve a collation NAME to its loaded table (spec/design/collation.md §1). `C` is the built-in
/// byte / code-point order → `None` (the unchanged fast path); any other name must be loaded
/// (`db.import_collation`), else 42704.
fn resolve_collation_name(
    catalog: &Engine,
    name: &str,
) -> Result<Option<std::sync::Arc<Collation>>> {
    if name == "C" {
        return Ok(None);
    }
    match catalog.read_snap().resolve_collation(name) {
        Some(c) => Ok(Some(c)),
        None => Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("collation \"{name}\" does not exist"),
        )),
    }
}

/// A text expression's collation and its DERIVATION level (spec/design/collation.md §1, PostgreSQL's
/// rules). `None` ⇒ no collation (a non-text expr, or a bare literal — takes a neighbour's).
/// `Implicit(name)` ⇒ a column's frozen collation (`C` is a *distinct* implicit collation, so
/// `C`-vs-`en-US` conflicts — PG-matching). `Explicit(name)` ⇒ an explicit `COLLATE`.
/// `Indeterminate` ⇒ two different implicit collations met with no explicit override — an error only
/// when the collation is consumed (42P22, at `resolve_deriv`).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Deriv {
    None,
    Implicit(String),
    Explicit(String),
    Indeterminate,
}

/// Derive the collation + derivation level of a (text) expression subtree (spec/design/collation.md
/// §1). A `COLLATE` is explicit; a column reference is implicit (its frozen collation, `C` if none);
/// `||` combines its operands. Every other shape (literal, cast, function, CASE) resets to `None`
/// (no collation — takes a neighbour's), a documented narrowing (collation.md §14).
fn derive_collation(scope: &Scope, e: &Expr) -> Result<Deriv> {
    Ok(match e {
        Expr::Collate { collation, .. } => Deriv::Explicit(collation.clone()),
        Expr::Column(name) => column_deriv(scope, scope.resolve_bare(name).ok()),
        Expr::QualifiedColumn { qualifier, name } => {
            column_deriv(scope, scope.resolve_qualified(qualifier, name).ok())
        }
        Expr::Binary {
            op: BinaryOp::Concat,
            lhs,
            rhs,
        } => combine_deriv(derive_collation(scope, lhs)?, derive_collation(scope, rhs)?)?,
        _ => Deriv::None,
    })
}

/// The implicit derivation of a resolved column reference: a text column carries `Implicit(name)`
/// (its frozen collation, `C` if none); a non-text column or an unresolvable reference is `None`.
fn column_deriv(scope: &Scope, r: Option<Resolved>) -> Deriv {
    match r {
        Some(r) => {
            let col = scope.column_of(r);
            if col.ty.is_text() {
                Deriv::Implicit(col.collation.clone().unwrap_or_else(|| "C".to_string()))
            } else {
                Deriv::None
            }
        }
        None => Deriv::None,
    }
}

/// Combine two operands' derivations (spec/design/collation.md §1/§7, PG's rules). Explicit
/// dominates; two DIFFERENT explicit collations conflict eagerly (42P21); two different implicit
/// collations yield `Indeterminate` (deferred to 42P22 on use); explicit resolves an indeterminacy.
fn combine_deriv(a: Deriv, b: Deriv) -> Result<Deriv> {
    use Deriv::*;
    Ok(match (a, b) {
        (Explicit(x), Explicit(y)) => {
            if x != y {
                return Err(EngineError::new(
                    SqlState::CollationMismatch,
                    format!("collation mismatch between explicit collations \"{x}\" and \"{y}\""),
                ));
            }
            Explicit(x)
        }
        (Explicit(x), _) | (_, Explicit(x)) => Explicit(x),
        (Indeterminate, _) | (_, Indeterminate) => Indeterminate,
        (Implicit(x), Implicit(y)) => {
            if x == y {
                Implicit(x)
            } else {
                Indeterminate
            }
        }
        (Implicit(x), None) | (None, Implicit(x)) => Implicit(x),
        (None, None) => None,
    })
}

/// Resolve a derivation to the concrete collation a comparison / ORDER BY uses (spec/design/
/// collation.md §1/§7). `None` and `C` ⇒ `None` (byte order, the fast path); a loaded name ⇒ its
/// table (42704 if it vanished); `Indeterminate` ⇒ 42P22 (the collation is required but ambiguous).
fn resolve_deriv(catalog: &Engine, d: Deriv) -> Result<Option<std::sync::Arc<Collation>>> {
    match d {
        Deriv::None => Ok(None),
        Deriv::Implicit(name) | Deriv::Explicit(name) => resolve_collation_name(catalog, &name),
        Deriv::Indeterminate => Err(EngineError::new(
            SqlState::IndeterminateCollation,
            "could not determine which collation to use for string comparison",
        )),
    }
}

/// Compare two non-NULL text values under a loaded collation (spec/design/collation.md §6/§7): order
/// by the UCA sort keys, whose `memcmp` order IS the collation order. The caller charges the
/// `collate` cost and handles NULLs.
fn collated_cmp(coll: &Collation, a: &str, b: &str) -> Result<std::cmp::Ordering> {
    let ka = collation::sort_key(coll, a)?;
    let kb = collation::sort_key(coll, b)?;
    Ok(ka.cmp(&kb))
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
            // jsonb `-` is the delete operator (json-sql-functions.md §1, J6), NOT arithmetic — its
            // right operand is a key/index/keys, never an arithmetic value. Peek the LHS type; a
            // jsonb LHS with `-` routes to the delete resolver. (Only `-` has a jsonb meaning; `+ *
            // / %` over a jsonb operand fall through and 42804 in the numeric path.)
            if matches!(op, BinaryOp::Sub) {
                let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
                if matches!(lt, ResolvedType::Jsonb) {
                    return resolve_jsonb_delete(scope, false, lhs, rhs, rl, agg, params);
                }
            }
            // Arithmetic is overloaded across integer and decimal. Resolve the operand pair
            // (an integer literal adapts to an integer sibling), then pick the family: both
            // integer → integer arithmetic (promotion tower); at least one decimal → decimal
            // arithmetic (the integer operand widens at eval); a text/boolean operand is a
            // 42804 (spec/design/decimal.md §4).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            // Range set operators (RF4, spec/design/range-functions.md §4): `+` union, `-`
            // difference, `*` intersection over two ranges. A range operand in any of these three is
            // the set-op axis — both operands must be ranges of a common element type, else 42883
            // (matching PG's "operator does not exist"); the numeric/temporal arithmetic below never
            // sees a range. `/` and `%` have no range meaning and fall straight through.
            if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                && (matches!(lt, ResolvedType::Range(_)) || matches!(rt, ResolvedType::Range(_)))
            {
                return resolve_range_set_op(op, rl, lt, rr, rt);
            }
            // Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32
            // (days between), date ± interval → timestamp. Checked BEFORE the interval/timestamp
            // rules below: a `date ± interval` pair has an interval operand, which would otherwise
            // make `temporal_arith_result` report a 42804 (date is not one of its temporal types).
            // Any other arithmetic combination involving a date is a 42804 from `date_arith_result`.
            if matches!(lt, ResolvedType::Date) || matches!(rt, ResolvedType::Date) {
                let result = date_arith_result(op, &lt, &rt)?;
                let aop = if matches!(op, BinaryOp::Add) {
                    ArithOp::Add
                } else {
                    ArithOp::Sub
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5).
            // interval * number, number * interval (commute), interval / number. Checked before
            // the ±-only temporal rule below.
            if let Some(res) = interval_scale_result(op, &lt, &rt) {
                let result = res?;
                let aop = if matches!(op, BinaryOp::Mul) {
                    ArithOp::Mul
                } else {
                    ArithOp::Div
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz]
            // ± interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval.
            // The eval dispatches on the value kinds; here we settle the result type. A temporal
            // operand in any other combination is a 42804.
            if let Some(res) = temporal_arith_result(op, &lt, &rt) {
                let result = res?;
                let aop = if matches!(op, BinaryOp::Add) {
                    ArithOp::Add
                } else {
                    ArithOp::Sub
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // Float arithmetic (spec/design/float.md §5): float ⊕ float → float, mixed widths
            // PROMOTE to f64 first (the implicit f32 → f64 cast). A float paired with
            // any non-float family is a 42804 (the strict island), reported by require_numeric
            // below since one side is Float. A pure float pair (or float × NULL) is handled here.
            if matches!(lt, ResolvedType::Float(_)) || matches!(rt, ResolvedType::Float(_)) {
                match promote_float_arith(rl, lt, rr, rt) {
                    Some((rl, rr, result)) => {
                        let aop = match op {
                            BinaryOp::Add => ArithOp::Add,
                            BinaryOp::Sub => ArithOp::Sub,
                            BinaryOp::Mul => ArithOp::Mul,
                            BinaryOp::Div => ArithOp::Div,
                            BinaryOp::Mod => ArithOp::Mod,
                            _ => unreachable!(),
                        };
                        return Ok((
                            RExpr::Arith {
                                op: aop,
                                lhs: Box::new(rl),
                                rhs: Box::new(rr),
                                result,
                            },
                            ResolvedType::Float(result),
                        ));
                    }
                    // A float paired with a non-float, non-NULL family — the strict island
                    // (int/decimal × float is 42804, spec/design/float.md §6).
                    None => {
                        return Err(type_error("arithmetic operators require numeric operands"));
                    }
                }
            }
            require_numeric_operand(&lt)?;
            require_numeric_operand(&rt)?;
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
                let p = promote(&lt, &rt);
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
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            // Comparison is overloaded across families: integer×integer or text×text.
            // Resolve the operands (a literal adapts to its sibling; text literals stay
            // text), then require they be comparable — a mixed integer/text pair is 42804.
            // The runtime comparison (eq3/lt3/gt3) dispatches on the value variants.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(&lt, &rt)?;
            // A mixed-width float comparison promotes the f32 side to f64 first (the
            // implicit cast — spec/design/float.md §2/§3), so the runtime compare sees one width.
            let (rl, rr) =
                if matches!(lt, ResolvedType::Float(_)) && matches!(rt, ResolvedType::Float(_)) {
                    (widen_float_to_f64(rl, &lt), widen_float_to_f64(rr, &rt))
                } else {
                    (rl, rr)
                };
            let cop = match op {
                BinaryOp::Eq => CmpOp::Eq,
                BinaryOp::Ne => CmpOp::Ne,
                BinaryOp::Lt => CmpOp::Lt,
                BinaryOp::Gt => CmpOp::Gt,
                BinaryOp::Le => CmpOp::Le,
                BinaryOp::Ge => CmpOp::Ge,
                _ => unreachable!(),
            };
            // Derive the comparison's collation (spec/design/collation.md §1/§7). Only a text×text
            // comparison is collatable; for any other operand family collation is irrelevant (and a
            // COLLATE on a non-text operand was already rejected 42804 at the Collate node). Each
            // operand's derivation (explicit COLLATE / implicit column collation / none) is combined
            // per PG's rules: two different EXPLICIT collations conflict (42P21); two different
            // IMPLICIT collations are indeterminate (42P22 when consumed here). The derivation runs
            // for ALL comparison ops including `=`/`<>` (PG raises the conflict regardless), even
            // though `=`/`<>` ignore the collation at eval (byte equality, §7).
            let collation = if matches!(lt, ResolvedType::Text) && matches!(rt, ResolvedType::Text)
            {
                let d =
                    combine_deriv(derive_collation(scope, lhs)?, derive_collation(scope, rhs)?)?;
                resolve_deriv(scope.catalog, d)?
            } else {
                None
            };
            Ok((
                RExpr::Compare {
                    op: cop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    collation,
                },
                ResolvedType::Bool,
            ))
        }
        BinaryOp::And | BinaryOp::Or => {
            let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
            let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
            require_bool(&lt, "AND/OR requires boolean operands")?;
            require_bool(&rt, "AND/OR requires boolean operands")?;
            let node = if matches!(op, BinaryOp::And) {
                RExpr::And(Box::new(rl), Box::new(rr))
            } else {
                RExpr::Or(Box::new(rl), Box::new(rr))
            };
            Ok((node, ResolvedType::Bool))
        }
        BinaryOp::Concat => resolve_concat(scope, lhs, rhs, agg, params),
        // The containment/overlap operators (@>/<@/&&, shared by arrays and ranges) and the five
        // range-only positional/adjacency operators (<</>>/&</&>/-|-) all dispatch here: the operand
        // type chooses the array axis (array-functions.md §10) or the range axis (range-functions.md §3).
        BinaryOp::Contains
        | BinaryOp::ContainedBy
        | BinaryOp::Overlaps
        | BinaryOp::StrictlyLeft
        | BinaryOp::StrictlyRight
        | BinaryOp::NotExtendRight
        | BinaryOp::NotExtendLeft
        | BinaryOp::Adjacent => resolve_set_op(scope, op, lhs, rhs, agg, params),
        // The jsonb accessor operators (spec/design/json-sql-functions.md §1, J4).
        BinaryOp::JsonGet
        | BinaryOp::JsonGetText
        | BinaryOp::JsonGetPath
        | BinaryOp::JsonGetPathText => resolve_json_access(scope, op, lhs, rhs, agg, params),
        // The jsonb key-existence operators (spec/design/json-sql-functions.md §1, J5).
        BinaryOp::JsonHasKey => resolve_json_has_key(scope, HasKeyKind::One, lhs, rhs, agg, params),
        BinaryOp::JsonHasAnyKey => {
            resolve_json_has_key(scope, HasKeyKind::Any, lhs, rhs, agg, params)
        }
        BinaryOp::JsonHasAllKeys => {
            resolve_json_has_key(scope, HasKeyKind::All, lhs, rhs, agg, params)
        }
        // The jsonb delete-at-path operator `#-` (spec/design/json-sql-functions.md §1, J6). `||`
        // and `-` (delete) are dispatched by operand type in resolve_concat / the arithmetic arm.
        BinaryOp::JsonDeletePath => {
            let (rbase, base_ty) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
            match base_ty {
                ResolvedType::Jsonb | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        format!("operator does not exist: {} #- text[]", base_ty.type_name()),
                    ));
                }
            }
            resolve_jsonb_delete(scope, true, lhs, rhs, rbase, agg, params)
        }
        // `jsonb @? jsonpath` = jsonb_path_exists, `jsonb @@ jsonpath` = jsonb_path_match
        // (jsonpath.md §6). Both reuse the jsonpath kernels.
        BinaryOp::JsonPathExists | BinaryOp::JsonPathMatch => {
            let (sym, kind) = if matches!(op, BinaryOp::JsonPathExists) {
                ("@?", JsonPathFnKind::Exists)
            } else {
                ("@@", JsonPathFnKind::Match)
            };
            let (ctx, ct) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
            if !matches!(ct, ResolvedType::Jsonb | ResolvedType::Null) {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!("operator does not exist: {} {sym} jsonpath", ct.type_name()),
                ));
            }
            let (path, pt) = resolve(scope, rhs, Some(ScalarType::JsonPath), agg, params)?;
            if !matches!(pt, ResolvedType::JsonPath | ResolvedType::Null) {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!("operator does not exist: jsonb {sym} (a non-jsonpath)"),
                ));
            }
            Ok((
                RExpr::JsonPathFn {
                    kind,
                    args: vec![ctx, path],
                },
                ResolvedType::Bool,
            ))
        }
    }
}

/// Resolve a jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1). The
/// base must be `jsonb` (a `json` base is the deferred 0A000 follow-on — json.md §4; any other base
/// is 42883). For `->`/`->>` the argument is a key (`text`) or an array index (`integer`); for
/// `#>`/`#>>` it is a `text[]` path (a bare string literal `'{a,b}'` adapts via `array_in`). The
/// result is `jsonb` (`-> #>`) or `text` (`->> #>>`); a missing access yields SQL NULL at eval.
fn resolve_json_access(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (rbase, base_ty) = resolve(scope, lhs, None, agg, params)?;
    // The base must be jsonb. json is a documented deferred follow-on (its operators preserve the
    // verbatim sub-text — json.md §4); any other base type has no such operator (42883).
    match base_ty {
        ResolvedType::Jsonb => {}
        ResolvedType::Json => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "json accessor operators are not supported yet; cast to jsonb",
            ));
        }
        ResolvedType::Null => {} // a NULL base propagates (the access is NULL)
        _ => {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {} ...",
                    base_ty.type_name(),
                    json_op_symbol(op)
                ),
            ));
        }
    }
    let (jop, result, path) = match op {
        BinaryOp::JsonGet => (JsonGetOp::Arrow, ResolvedType::Jsonb, false),
        BinaryOp::JsonGetText => (JsonGetOp::ArrowText, ResolvedType::Text, false),
        BinaryOp::JsonGetPath => (JsonGetOp::HashArrow, ResolvedType::Jsonb, true),
        BinaryOp::JsonGetPathText => (JsonGetOp::HashArrowText, ResolvedType::Text, true),
        _ => unreachable!("resolve_json_access only handles the four accessor operators"),
    };
    let rarg = if path {
        // `#>` / `#>>` take a text[] path. A bare string literal `'{a,b}'` adapts via array_in;
        // otherwise the resolved argument must be a text[] (else 42883).
        if let Expr::Literal(Literal::Text(s)) = rhs {
            let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
            value_to_rexpr(&val)
        } else {
            let (rarg, arg_ty) = resolve(scope, rhs, None, agg, params)?;
            match arg_ty {
                ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => {}
                ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        "the #> / #>> path argument must be text[]",
                    ));
                }
            }
            rarg
        }
    } else {
        // `->` / `->>` take a key (text) or an array index (integer). A string literal stays text;
        // an integer literal stays integer; no adaptation is needed.
        let (rarg, arg_ty) = resolve(scope, rhs, None, agg, params)?;
        match arg_ty {
            ResolvedType::Text | ResolvedType::Int(_) | ResolvedType::Null => {}
            _ => {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: jsonb {} {}",
                        json_op_symbol(op),
                        arg_ty.type_name()
                    ),
                ));
            }
        }
        rarg
    };
    Ok((
        RExpr::JsonGet {
            op: jop,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        result,
    ))
}

/// The node tree of a json/jsonb function argument: a `jsonb` value IS the canonical node; a `json`
/// value is parsed from its verbatim text on demand, preserving key order + duplicates (json.md §4).
fn json_arg_node(v: &Value) -> Result<JsonNode> {
    match v {
        Value::Jsonb(n) => Ok(n.clone()),
        Value::Json(s) => json::parse_preserving(s),
        _ => unreachable!("resolver restricts a json/jsonb function argument to json/jsonb"),
    }
}

/// Whether a parsed JSON node matches an `IS JSON [kind]` predicate's kind (json-sql-functions.md §5).
fn json_pred_kind_matches(node: &JsonNode, kind: JsonPredicateKind) -> bool {
    match kind {
        JsonPredicateKind::Value => true,
        JsonPredicateKind::Scalar => !matches!(node, JsonNode::Object(_) | JsonNode::Array(_)),
        JsonPredicateKind::Array => matches!(node, JsonNode::Array(_)),
        JsonPredicateKind::Object => matches!(node, JsonNode::Object(_)),
    }
}

/// The JSON image of any value — the `to_jsonb` kernel (json-sql-functions.md §2), also reused by
/// the json aggregates (B4). Numbers stay exact (`decimal`, never float); a `json`/`jsonb` value
/// canonicalizes; a 1-D array maps to a JSON array recursively (a NULL element → JSON null). The
/// type-info-dependent / float-divergent sources — composite (needs field names), float (the
/// binary→decimal divergence), datetime/uuid/bytea/interval (string-render divergences), and a
/// multidimensional array — are a deferred `0A000` follow-on.
fn value_to_node(v: &Value) -> Result<JsonNode> {
    Ok(match v {
        Value::Null => JsonNode::Null, // an array element (a top-level NULL is strict-propagated)
        Value::Bool(b) => JsonNode::Bool(*b),
        Value::Int(n) => JsonNode::Number(Decimal::from_i64(*n)),
        Value::Decimal(d) => JsonNode::Number(d.clone()),
        Value::Text(s) => JsonNode::String(s.clone()),
        Value::Jsonb(n) => n.clone(),
        Value::Json(s) => json::jsonb_in(s)?,
        Value::Array(arr) => {
            if arr.ndim() > 1 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "to_jsonb of a multidimensional array is not supported yet",
                ));
            }
            let mut elems = Vec::with_capacity(arr.elements.len());
            for e in &arr.elements {
                elems.push(value_to_node(e)?);
            }
            JsonNode::Array(elems)
        }
        Value::Float32(_) | Value::Float64(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a float value is not supported yet",
            ));
        }
        Value::Composite(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a composite value is not supported yet",
            ));
        }
        Value::Uuid(_)
        | Value::Date(_)
        | Value::Timestamp(_)
        | Value::Timestamptz(_)
        | Value::Interval(_)
        | Value::Bytea(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of this type is not supported yet",
            ));
        }
        Value::Range(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a range value is not supported yet",
            ));
        }
        Value::JsonPath(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a jsonpath value is not supported yet",
            ));
        }
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    })
}

/// One element's `json`-builder text image (json-sql-functions.md §2): a `json` value embeds VERBATIM,
/// a `jsonb` value its canonical (spaced) render, everything else the compact `to_jsonb` image. This
/// is how PG's `json_build_array`/`json_build_object` embed an argument's own json form.
fn elem_json_text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Json(s) => s.clone(),
        Value::Jsonb(n) => json::jsonb_out(n),
        _ => json::json_compact_out(&value_to_node(v)?),
    })
}

/// The text form of a `json[b]_build_object` KEY argument (1-based `pos` for the error message). PG
/// coerces a key to text via the type's output: text as-is, integer/decimal/boolean rendered. A NULL
/// key is `22023`; a non-scalar key type is a deferred `0A000` follow-on.
fn object_key_text(v: &Value, pos: usize) -> Result<String> {
    Ok(match v {
        Value::Null => {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("argument {pos}: key must not be null"),
            ));
        }
        Value::Text(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.render(),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        _ => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a json_build_object key of this type is not supported yet",
            ));
        }
    })
}

/// The `22004` raised when a `json_object` / `jsonb_object` key element is NULL.
fn object_key_null() -> EngineError {
    EngineError::new(
        SqlState::NullValueNotAllowed,
        "null value not allowed for object key",
    )
}

/// The display symbol for a jsonb accessor operator, for error messages.
fn json_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::JsonGetPath => "#>",
        BinaryOp::JsonGetPathText => "#>>",
        _ => "?",
    }
}

/// The "operator does not exist" error (42883) for a containment/positional operator whose operands
/// are neither arrays of a common element type nor ranges of a common element type (matches PG).
fn no_set_op_overload() -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        "operator does not exist: the operands are not arrays or ranges of a common element type",
    )
}

/// Resolve a containment / overlap / positional operator (`@>` `<@` `&&` `<<` `>>` `&<` `&>` `-|-`),
/// choosing the axis by operand type: an array operand → the array containment surface
/// (array-functions.md §10, only `@>`/`<@`/`&&`); a range operand → the range boolean surface
/// (range-functions.md §3). The result is always boolean (strict — a NULL operand short-circuits to
/// NULL at eval). A non-array / non-range pair, or a positional operator on arrays, is 42883.
fn resolve_set_op(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Pass 1: resolve both operands with no hint.
    let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
    let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
    // RANGE axis if either operand is a range. (The five positional operators are range-only; on a
    // non-range pair they fall through to the array branch below, which rejects them as 42883.)
    if matches!(lt, ResolvedType::Range(_)) || matches!(rt, ResolvedType::Range(_)) {
        return resolve_range_op(scope, op, lhs, rhs, rl, lt, rr, rt, agg, params);
    }

    // JSONB axis: only @>/<@ have a jsonb overload (json-sql-functions.md §1, J5). A jsonb operand
    // (or a string literal adapting to one) routes here; `&&`/the positional operators have no jsonb
    // overload and fall through to the array branch (42883). A json operand has no @> opclass (42883).
    if (matches!(op, BinaryOp::Contains | BinaryOp::ContainedBy))
        && (matches!(lt, ResolvedType::Jsonb) || matches!(rt, ResolvedType::Jsonb))
    {
        return resolve_jsonb_contains(scope, op, lhs, rhs, agg, params);
    }

    // ARRAY axis: only @>/<@/&& have an array overload (array-functions.md §10).
    let func = match op {
        BinaryOp::Contains => ArrayFunc::Contains,
        BinaryOp::ContainedBy => ArrayFunc::ContainedBy,
        BinaryOp::Overlaps => ArrayFunc::Overlaps,
        // A positional/adjacency operator on non-range operands — no array overload exists.
        _ => return Err(no_set_op_overload()),
    };
    let (mut rl, mut lt) = (rl, lt);
    let (mut rr, mut rt) = (rr, rt);
    // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8), so a
    // bare `ARRAY[…]` constructor adapts to the column's element type (`xs @> ARRAY[20]`).
    let hint = match (&lt, &rt) {
        (ResolvedType::Array(e), _) => elem_scalar_hint(e),
        (_, ResolvedType::Array(e)) => elem_scalar_hint(e),
        _ => None,
    };
    // Pass 2: re-resolve the NON-NULL operands with the hint. A bare NULL (pass-1 type `Null`) is
    // left untyped — it defers in the anyarray slot and the boolean result is unaffected.
    if let Some(s) = hint {
        if !matches!(lt, ResolvedType::Null) {
            (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
        }
        if !matches!(rt, ResolvedType::Null) {
            (rr, rt) = resolve(scope, rhs, Some(s), agg, params)?;
        }
    }

    // Both slots are `anyarray`: the element types must unify (a non-array / mismatch is 42883).
    let tys = [lt, rt];
    match_poly(&["anyarray", "anyarray"], &tys).ok_or_else(no_set_op_overload)?;
    Ok((
        RExpr::ArrayFunc {
            func,
            args: vec![rl, rr],
        },
        ResolvedType::Bool,
    ))
}

/// Resolve a jsonb containment operator `@>` / `<@` (json-sql-functions.md §1, J5). Both operands
/// must be `jsonb` (a bare string literal adapts via `jsonb_in`); a `json` operand has no @>
/// operator class (42883). `<@` resolves to `JsonContains` with the operands swapped (`a <@ b` is
/// `b @> a`). The result is boolean; the operator is strict (a NULL operand yields SQL NULL).
fn resolve_jsonb_contains(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Resolve each operand with a jsonb context, so a bare `'{"a":1}'` string literal adapts.
    let resolve_jsonb = |e: &Expr, agg: &mut AggCtx, params: &mut ParamTypes| -> Result<RExpr> {
        let (r, t) = resolve(scope, e, Some(ScalarType::Jsonb), agg, params)?;
        match t {
            ResolvedType::Jsonb | ResolvedType::Null => Ok(r),
            _ => Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {} {}",
                    t.type_name(),
                    binary_op_symbol(op),
                    "jsonb"
                ),
            )),
        }
    };
    let rl = resolve_jsonb(lhs, agg, params)?;
    let rr = resolve_jsonb(rhs, agg, params)?;
    // `a @> b` keeps the order; `a <@ b` is `b @> a`.
    let (a, b) = match op {
        BinaryOp::Contains => (rl, rr),
        BinaryOp::ContainedBy => (rr, rl),
        _ => unreachable!("resolve_jsonb_contains only handles @> / <@"),
    };
    Ok((
        RExpr::JsonContains {
            a: Box::new(a),
            b: Box::new(b),
        },
        ResolvedType::Bool,
    ))
}

/// Resolve a jsonb key-existence operator `?` / `?|` / `?&` (json-sql-functions.md §1, J5). The base
/// must be `jsonb` (a json base is 42883 — no operator). `?` takes a `text` key; `?|`/`?&` take a
/// `text[]` (a bare `'{a,b}'` string literal adapts). The result is boolean; the operator is strict.
fn resolve_json_has_key(
    scope: &Scope,
    kind: HasKeyKind,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (rbase, base_ty) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
    match base_ty {
        ResolvedType::Jsonb | ResolvedType::Null => {}
        _ => {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {}",
                    base_ty.type_name(),
                    has_key_symbol(kind)
                ),
            ));
        }
    }
    let rarg = match kind {
        HasKeyKind::One => {
            // `?` takes a single text key.
            let (r, t) = resolve(scope, rhs, Some(ScalarType::Text), agg, params)?;
            match t {
                ResolvedType::Text | ResolvedType::Null => r,
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        "the ? operator's right argument must be text",
                    ));
                }
            }
        }
        HasKeyKind::Any | HasKeyKind::All => {
            // `?|` / `?&` take a text[] (a bare string literal adapts via array_in).
            if let Expr::Literal(Literal::Text(s)) = rhs {
                let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
                value_to_rexpr(&val)
            } else {
                let (r, t) = resolve(scope, rhs, None, agg, params)?;
                match t {
                    ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => r,
                    ResolvedType::Null => r,
                    _ => {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "the ?| / ?& operator's right argument must be text[]",
                        ));
                    }
                }
            }
        }
    };
    Ok((
        RExpr::JsonHasKey {
            kind,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        ResolvedType::Bool,
    ))
}

/// The display symbol for a key-existence operator, for error messages.
fn has_key_symbol(kind: HasKeyKind) -> &'static str {
    match kind {
        HasKeyKind::One => "?",
        HasKeyKind::Any => "?|",
        HasKeyKind::All => "?&",
    }
}

/// Resolve a jsonb `||` concatenation/merge (json-sql-functions.md §1, J6). Both operands must be
/// jsonb (a string literal adapts via `jsonb_in`). Result jsonb; strict.
fn resolve_jsonb_concat(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let resolve_jsonb = |e: &Expr, agg: &mut AggCtx, params: &mut ParamTypes| -> Result<RExpr> {
        let (r, t) = resolve(scope, e, Some(ScalarType::Jsonb), agg, params)?;
        match t {
            ResolvedType::Jsonb | ResolvedType::Null => Ok(r),
            _ => Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("operator does not exist: {} || jsonb", t.type_name()),
            )),
        }
    };
    let a = resolve_jsonb(lhs, agg, params)?;
    let b = resolve_jsonb(rhs, agg, params)?;
    Ok((
        RExpr::JsonConcat {
            a: Box::new(a),
            b: Box::new(b),
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve a jsonb delete operator: `-` (key `text` / index `int` / keys `text[]`) or `#-` (path
/// `text[]`) — json-sql-functions.md §1, J6. The base must be jsonb (a json base is 42883). The
/// form is chosen by the argument type; a bare `'{a,b}'` string literal adapts to `text[]`. Result
/// jsonb; strict.
fn resolve_jsonb_delete(
    scope: &Scope,
    is_path: bool,
    lhs: &Expr,
    rhs: &Expr,
    rbase: RExpr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let sym = if is_path { "#-" } else { "-" };
    let _ = lhs;
    // `#-` always takes a text[] path; `-` takes text / int / text[].
    let (kind, rarg) = if is_path {
        (
            DeleteKind::Path,
            resolve_text_array_arg(scope, rhs, sym, agg, params)?,
        )
    } else if let Expr::Literal(Literal::Text(_)) = rhs {
        // A bare string literal is a text key (`jsonb - 'a'`), NOT a text[].
        let (r, _) = resolve(scope, rhs, Some(ScalarType::Text), agg, params)?;
        (DeleteKind::Key, r)
    } else {
        let (r, t) = resolve(scope, rhs, None, agg, params)?;
        match t {
            ResolvedType::Text | ResolvedType::Null => (DeleteKind::Key, r),
            ResolvedType::Int(_) => (DeleteKind::Index, r),
            ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => {
                (DeleteKind::Keys, r)
            }
            _ => {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: jsonb - {} (expected text, integer, or text[])",
                        t.type_name()
                    ),
                ));
            }
        }
    };
    Ok((
        RExpr::JsonDelete {
            kind,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve a `text[]` operator argument (the `#-` path / the `?|`/`?&` style): a bare string literal
/// `'{a,b}'` adapts via `array_in`; otherwise the resolved type must be `text[]` (or NULL). `sym` is
/// the operator symbol for the error message.
fn resolve_text_array_arg(
    scope: &Scope,
    rhs: &Expr,
    sym: &str,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    if let Expr::Literal(Literal::Text(s)) = rhs {
        let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
        return Ok(value_to_rexpr(&val));
    }
    let (r, t) = resolve(scope, rhs, None, agg, params)?;
    match t {
        ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => Ok(r),
        ResolvedType::Null => Ok(r),
        _ => Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("the {sym} operator's right argument must be text[]"),
        )),
    }
}

/// Resolve `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): `(target jsonb, path text[],
/// value jsonb [, flag boolean])` → jsonb. A bare `'{a,b}'` path literal adapts to text[] and a bare
/// string `value` literal adapts to jsonb. STRICT (the eval propagates any NULL). The optional flag
/// defaults to `true` for jsonb_set (create_if_missing) / `false` for jsonb_insert (insert_after).
fn resolve_jsonb_set_insert(
    scope: &Scope,
    name: &str,
    mode: json::PathSetMode,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 3 && args.len() != 4 {
        return Err(no_func_overload(name));
    }
    let (target, t0) = resolve(scope, &args[0], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(t0, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let path = resolve_text_array_arg(scope, &args[1], name, agg, params)?;
    let (value, t2) = resolve(scope, &args[2], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(t2, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let flag = if args.len() == 4 {
        let (f, tf) = resolve(scope, &args[3], Some(ScalarType::Bool), agg, params)?;
        if !matches!(tf, ResolvedType::Bool | ResolvedType::Null) {
            return Err(no_func_overload(name));
        }
        f
    } else {
        // Default: jsonb_set create_if_missing = true; jsonb_insert insert_after = false.
        value_to_rexpr(&Value::Bool(mode == json::PathSetMode::Set))
    };
    Ok((
        RExpr::JsonSetInsert {
            mode,
            args: vec![target, path, value, flag],
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve `json_object` / `jsonb_object` (json-sql-functions.md §2): one `text[]` of alternating
/// keys/values, or two `text[]` (keys, values). A bare `'{…}'` literal adapts to text[]. STRICT (the
/// eval propagates a NULL whole-array argument).
fn resolve_json_object(
    scope: &Scope,
    name: &str,
    json: bool,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.is_empty() || args.len() > 2 {
        return Err(no_func_overload(name));
    }
    let mut rargs = Vec::with_capacity(args.len());
    for a in args {
        rargs.push(resolve_text_array_arg(scope, a, name, agg, params)?);
    }
    let result = if json {
        ResolvedType::Json
    } else {
        ResolvedType::Jsonb
    };
    Ok((RExpr::JsonObjectFromArrays { json, args: rargs }, result))
}

/// Resolve a scalar jsonpath query function (P2, jsonpath.md §5): `(ctx jsonb, path jsonpath)`. A
/// bare string literal adapts (the context to jsonb, the path to a compiled jsonpath). STRICT.
fn resolve_jsonpath_fn(
    scope: &Scope,
    name: &str,
    kind: JsonPathFnKind,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (ctx, path) = resolve_jsonpath_args(scope, name, args, agg, params)?;
    let result = match kind {
        JsonPathFnKind::Exists | JsonPathFnKind::Match => ResolvedType::Bool,
        JsonPathFnKind::QueryFirst | JsonPathFnKind::QueryArray => ResolvedType::Jsonb,
    };
    Ok((
        RExpr::JsonPathFn {
            kind,
            args: vec![ctx, path],
        },
        result,
    ))
}

/// Resolve the `(context jsonb, path jsonpath)` argument pair shared by the jsonpath query functions
/// (the SRF and the scalar forms). A bare string literal adapts: the context to jsonb, the path to a
/// compiled `jsonpath`. Exactly two args this slice (the optional `vars` / `silent` are a follow-on).
fn resolve_jsonpath_args(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, RExpr)> {
    if args.len() != 2 {
        return Err(no_func_overload(name));
    }
    let (ctx, ct) = resolve(scope, &args[0], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(ct, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let (path, pt) = resolve(scope, &args[1], Some(ScalarType::JsonPath), agg, params)?;
    if !matches!(pt, ResolvedType::JsonPath | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    Ok((ctx, path))
}

/// Recompile a `jsonpath` value's canonical text and evaluate it over a `jsonb` context value (the
/// shared kernel of the jsonpath query functions). A NULL context or path yields `None` (→ SQL NULL).
fn eval_jsonpath(ctx: &Value, path: &Value) -> Result<Option<Vec<JsonNode>>> {
    let node = match ctx {
        Value::Null => return Ok(None),
        _ => json_arg_node(ctx)?,
    };
    let text = match path {
        Value::Null => return Ok(None),
        Value::JsonPath(s) => s,
        _ => unreachable!("resolver restricts a jsonpath argument to jsonpath"),
    };
    let compiled = crate::jsonpath::JsonPath::compile(text)?;
    Ok(Some(crate::jsonpath::eval(&compiled, &node)?))
}

/// Extract a `text[]` value into `Vec<Option<String>>`, preserving NULL elements — `None` if the
/// value is not an array. Used by `json_object` (a NULL value → JSON null; a NULL key → 22004).
fn value_to_opt_text_array(v: &Value) -> Option<Vec<Option<String>>> {
    match v {
        Value::Array(arr) => Some(
            arr.elements
                .iter()
                .map(|e| match e {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Extract a `text[]` value into a path of strings — `None` if it is not an array or has a NULL
/// element (which propagates a SQL NULL through `jsonb_set`/`jsonb_insert`, like the `#-` path).
fn value_to_text_path(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) => {
            let mut path = Vec::with_capacity(arr.elements.len());
            for e in &arr.elements {
                match e {
                    Value::Text(s) => path.push(s.clone()),
                    _ => return None,
                }
            }
            Some(path)
        }
        _ => None,
    }
}

/// Map a containment/positional `BinaryOp` to its range-against-range kernel (`RangeOp`).
fn range_op_for(op: BinaryOp) -> RangeOp {
    match op {
        BinaryOp::Contains => RangeOp::Contains,
        BinaryOp::ContainedBy => RangeOp::ContainedBy,
        BinaryOp::Overlaps => RangeOp::Overlaps,
        BinaryOp::StrictlyLeft => RangeOp::Before,
        BinaryOp::StrictlyRight => RangeOp::After,
        BinaryOp::NotExtendRight => RangeOp::Overleft,
        BinaryOp::NotExtendLeft => RangeOp::Overright,
        BinaryOp::Adjacent => RangeOp::Adjacent,
        _ => unreachable!("range_op_for is only called for the eight set/positional operators"),
    }
}

/// Resolve the RANGE axis of a containment/positional operator (range-functions.md §3), with both
/// operands already resolved (pass 1). The overload is chosen by the operand types: range×range (the
/// elements must match, else 42883) for every operator; the bare element overloads `range @> element`
/// and `element <@ range` re-resolve the element operand with the range's element type as the hint and
/// type-check assignability. A bare untyped `NULL` on one side is treated as a NULL range (the
/// range×range overload; eval yields NULL). Anything else is 42883.
#[allow(clippy::too_many_arguments)]
fn resolve_range_op(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    match (&lt, &rt) {
        // range × range (or a bare NULL on one side, taken as a NULL range): the elements must match.
        (ResolvedType::Range(le), ResolvedType::Range(re)) => {
            if resolved_range_element_scalar(le) != resolved_range_element_scalar(re) {
                return Err(no_set_op_overload());
            }
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        (ResolvedType::Range(le), ResolvedType::Null) => {
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        (ResolvedType::Null, ResolvedType::Range(re)) => {
            let elem = resolved_range_element_scalar(re).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        // `range @> element` — the element overload of `@>` (the only operator with one). Re-resolve
        // the right operand with the range's element as the hint, then check it is assignable.
        (ResolvedType::Range(le), _) if op == BinaryOp::Contains => {
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            let (re_node, re_ty) = resolve(scope, rhs, Some(elem), agg, params)?;
            if !range_bound_assignable(&re_ty, elem) {
                return Err(no_set_op_overload());
            }
            Ok((
                RExpr::RangeOp {
                    op: RangeOp::ContainsElem,
                    args: vec![rl, re_node],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        // `element <@ range` — the element overload of `<@`.
        (_, ResolvedType::Range(re)) if op == BinaryOp::ContainedBy => {
            let elem = resolved_range_element_scalar(re).expect("a range element is scalar");
            let (le_node, le_ty) = resolve(scope, lhs, Some(elem), agg, params)?;
            if !range_bound_assignable(&le_ty, elem) {
                return Err(no_set_op_overload());
            }
            Ok((
                RExpr::RangeOp {
                    op: RangeOp::ElemContainedBy,
                    args: vec![le_node, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        _ => Err(no_set_op_overload()),
    }
}

/// Resolve a range SET operator (`+` union, `-` difference, `*` intersection — range-functions.md §4),
/// reached from [`resolve_binary`] when a `+`/`-`/`*` has a range operand (the operands are already
/// resolved). Both must be ranges over the SAME element type — a range × non-range, or a cross-element
/// pair, is `42883` (PG's "operator does not exist"); a bare untyped `NULL` beside a range is taken as
/// a NULL range (the range×range overload; eval → NULL, strict). The result is a range over that
/// element type. `range_merge` does NOT come through here (it is a function call — see
/// [`resolve_range_func`]); it shares the [`RExpr::RangeSetOp`] node with `op = Merge`.
fn resolve_range_set_op(
    op: BinaryOp,
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
) -> Result<(RExpr, ResolvedType)> {
    let elem = match (&lt, &rt) {
        (ResolvedType::Range(le), ResolvedType::Range(re)) => {
            let le = resolved_range_element_scalar(le).expect("a range element is scalar");
            let re = resolved_range_element_scalar(re).expect("a range element is scalar");
            if le != re {
                return Err(no_set_op_overload());
            }
            le
        }
        (ResolvedType::Range(le), ResolvedType::Null) => {
            resolved_range_element_scalar(le).expect("a range element is scalar")
        }
        (ResolvedType::Null, ResolvedType::Range(re)) => {
            resolved_range_element_scalar(re).expect("a range element is scalar")
        }
        // A range paired with a non-range (or any other combination) — no such operator.
        _ => return Err(no_set_op_overload()),
    };
    let setop = match op {
        BinaryOp::Add => RangeSetOp::Union,
        BinaryOp::Sub => RangeSetOp::Difference,
        BinaryOp::Mul => RangeSetOp::Intersect,
        _ => unreachable!("resolve_range_set_op is only called for +, -, *"),
    };
    Ok((
        RExpr::RangeSetOp {
            op: setop,
            args: vec![rl, rr],
        },
        ResolvedType::Range(Box::new(resolved_type_of(elem))),
    ))
}

/// Resolve a quantified array comparison `x op ANY/SOME/ALL(arr)` (array-functions.md §11): the
/// array spelling of `IN`. `x` (`lhs`) and the array operand resolve with the SAME literal
/// adaptation the comparison operators use — a bare-literal `x` adapts to the array's element type,
/// a bare `ARRAY[…]` operand adapts its elements to `x`'s type. The right operand must be an array
/// (a non-array side is `42809`; a bare untyped `NULL` is `42P18`); `x` and the element type must
/// be comparable (else `42883`, PG's operator-not-found). The result is always `boolean`; the 3VL
/// fold over the flattened elements reuses the `eq3`/`lt3`/`gt3` kernels at eval (the `IN`-list
/// membership machinery, generalized to all five operators and both quantifiers).
fn resolve_quantified(
    scope: &Scope,
    op: BinaryOp,
    all: bool,
    lhs: &Expr,
    array: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Pass 1: resolve both operands with no hint.
    let (mut rl, mut lt) = resolve(scope, lhs, None, agg, params)?;
    let (mut ra, mut at) = resolve(scope, array, None, agg, params)?;
    // If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
    // bare `ARRAY[…]` constructor, re-resolve the array with `x`'s type as the element hint so the
    // constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
    // column / cast operand (it ignores the hint).
    if !is_adaptable_operand(lhs) {
        if let Some(s) = ctx_of(&lt) {
            (ra, at) = resolve(scope, array, Some(s), agg, params)?;
        }
    }
    // If the array resolved to `E[]` and `x` is an adaptable bare literal, adapt `x` to `E` (with a
    // range check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
    if let ResolvedType::Array(e) = &at {
        if is_adaptable_operand(lhs) {
            if let Some(s) = elem_scalar_hint(e) {
                (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
            }
        }
    }
    // The right operand must be an array.
    let elem = match &at {
        ResolvedType::Array(e) => (**e).clone(),
        // A bare untyped NULL leaves the array type undeterminable — jed's polymorphic posture
        // (§11; the `unnest(NULL)` / §5 #6 precedent), a documented degenerate divergence from PG.
        ResolvedType::Null => {
            return Err(EngineError::new(
                SqlState::IndeterminateDatatype,
                "could not determine the array element type of a NULL ANY/ALL operand",
            ));
        }
        _ => {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                "op ANY/ALL (array) requires array on right side",
            ));
        }
    };
    // `x` and the element type must be comparable; PG reports operator-not-found (42883) here, NOT
    // the bare 42804 a plain `int = text` raises — matching AF4's element-mismatch posture (§10.2).
    classify_comparable(&lt, &elem).map_err(|_| {
        EngineError::new(
            SqlState::UndefinedFunction,
            format!(
                "operator does not exist: {} {} {}",
                lt.type_name(),
                binary_op_symbol(op),
                elem.type_name()
            ),
        )
    })?;
    let cop = match op {
        BinaryOp::Eq => CmpOp::Eq,
        BinaryOp::Ne => CmpOp::Ne,
        BinaryOp::Lt => CmpOp::Lt,
        BinaryOp::Gt => CmpOp::Gt,
        BinaryOp::Le => CmpOp::Le,
        BinaryOp::Ge => CmpOp::Ge,
        _ => unreachable!("the parser only builds a Quantified node for a comparison operator"),
    };
    Ok((
        RExpr::Quantified {
            op: cop,
            all,
            lhs: Box::new(rl),
            array: Box::new(ra),
        },
        ResolvedType::Bool,
    ))
}

/// The infix symbol for a comparison/arithmetic `BinaryOp`, for an `operator does not exist`
/// message (only the comparison operators reach `resolve_quantified`).
fn binary_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Concat => "||",
        BinaryOp::Contains => "@>",
        BinaryOp::ContainedBy => "<@",
        BinaryOp::Overlaps => "&&",
        BinaryOp::StrictlyLeft => "<<",
        BinaryOp::StrictlyRight => ">>",
        BinaryOp::NotExtendRight => "&<",
        BinaryOp::NotExtendLeft => "&>",
        BinaryOp::Adjacent => "-|-",
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::JsonGetPath => "#>",
        BinaryOp::JsonGetPathText => "#>>",
        BinaryOp::JsonHasKey => "?",
        BinaryOp::JsonHasAnyKey => "?|",
        BinaryOp::JsonHasAllKeys => "?&",
        BinaryOp::JsonDeletePath => "#-",
        BinaryOp::JsonPathExists => "@?",
        BinaryOp::JsonPathMatch => "@@",
    }
}

/// Resolve the `||` array concatenation operator (array-functions.md §8): overload resolution over
/// the three `concat` catalog rows — `(anyarray,anyarray)` [array_cat], `(anyarray,anyelement)`
/// [array_append], `(anyelement,anyarray)` [array_prepend] — tried IN CATALOG ORDER, first match
/// wins. It is the operator spelling of the AF1 builders and reuses their kernels.
///
/// Two passes like `resolve_array_func`, with one deliberate difference: a **bare untyped NULL**
/// operand is left un-adapted. `match_poly` defers a bare NULL in an `anyarray` slot, so cat-first
/// makes `arr || NULL` / `NULL || arr` resolve to array_cat (the NULL array = identity), matching
/// PostgreSQL; adapting the bare NULL to a typed element would wrongly steer it into array_append.
fn resolve_concat(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let no_overload = || {
        EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: the || operands are not an array and a compatible element/array",
        )
    };

    // Pass 1: resolve both operands with no hint.
    let (mut rl, mut lt) = resolve(scope, lhs, None, agg, params)?;
    let (mut rr, mut rt) = resolve(scope, rhs, None, agg, params)?;
    // JSONB axis: a jsonb operand routes `||` to jsonb concat/merge (json-sql-functions.md §1, J6).
    if matches!(lt, ResolvedType::Jsonb) || matches!(rt, ResolvedType::Jsonb) {
        return resolve_jsonb_concat(scope, lhs, rhs, agg, params);
    }
    // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
    let hint = match (&lt, &rt) {
        (ResolvedType::Array(e), _) => elem_scalar_hint(e),
        (_, ResolvedType::Array(e)) => elem_scalar_hint(e),
        _ => None,
    };
    // Pass 2: re-resolve the NON-NULL operands with the hint so a bare literal element / untyped
    // `ARRAY[…]` adapts to the array's element type. A bare NULL (pass-1 type `Null`) is skipped —
    // it must stay untyped so the cat-first overload order matches PG (see the doc comment).
    if let Some(s) = hint {
        if !matches!(lt, ResolvedType::Null) {
            (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
        }
        if !matches!(rt, ResolvedType::Null) {
            (rr, rt) = resolve(scope, rhs, Some(s), agg, params)?;
        }
    }

    // Try the three concat overloads in catalog order; the first whose slots unify wins.
    let tys = [lt, rt];
    let overload = OPERATORS
        .iter()
        .filter(|o| o.kind == "concat")
        .find_map(|o| match_poly(o.arg_families, &tys).map(|elem| (o, elem)));
    let (desc, elem) = overload.ok_or_else(no_overload)?;
    let result = poly_result_type(desc.result, &elem)?;
    // The matched overload's slot pattern selects the kernel; the operands stay in source order
    // (array_prepend's kernel already reads vals[0]=element, vals[1]=array).
    let func = match desc.arg_families {
        ["anyarray", "anyarray"] => ArrayFunc::ArrayCat,
        ["anyarray", "anyelement"] => ArrayFunc::ArrayAppend,
        ["anyelement", "anyarray"] => ArrayFunc::ArrayPrepend,
        _ => unreachable!("concat overload has an unexpected slot pattern"),
    };
    Ok((
        RExpr::ArrayFunc {
            func,
            args: vec![rl, rr],
        },
        result,
    ))
}

/// Resolve the two operands of a binary operator, giving each adaptable literal the other
/// operand's type as context: a bare *integer* literal adopts the sibling's integer type (so
/// `small + 1` types `1` as i16, and `small + 100000` traps 22003 at resolve), and a
/// *string* literal adapts to a bytea sibling (decoding the hex input — types.md §6/§13),
/// otherwise staying text. When the sibling offers no usable context, the literal defaults to
/// its own family and the caller's family check reports the mismatch. This does NOT enforce a
/// family — `resolve_int_pair`/arithmetic and `classify_comparable` (comparison) layer that on top.
/// Resolve a two-numeric scalar function (gcd/lcm) by reusing the arithmetic operand-pair
/// resolution (literal adaptation), then settling the result type. Both operands must be integer
/// or decimal (a float/other operand → 42883); the result is the promoted integer type when both
/// are integer, else `decimal` (an integer operand promotes, as PG does). The kernel reads the
/// result type to range-check an integer result (gcd's i64::MIN abs / lcm overflow).
fn resolve_int_or_decimal_pair(
    scope: &Scope,
    name: &str,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, RExpr, ScalarType)> {
    let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
    let numeric_ok = |t: &ResolvedType| {
        matches!(
            t,
            ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
        )
    };
    if !numeric_ok(&lt) || !numeric_ok(&rt) {
        return Err(no_func_overload(name));
    }
    let result = if matches!(lt, ResolvedType::Decimal) || matches!(rt, ResolvedType::Decimal) {
        ScalarType::Decimal
    } else {
        promote(&lt, &rt)
    };
    Ok((rl, rr, result))
}

/// A non-NULL integer/decimal value as a `Decimal` (the integer→decimal promotion gcd/lcm/div use).
fn value_to_decimal(v: &Value) -> Decimal {
    match v {
        Value::Int(n) => Decimal::from_i64(*n),
        Value::Decimal(d) => d.clone(),
        _ => unreachable!("expected an integer or decimal value"),
    }
}

/// gcd of two i64 by the Euclidean algorithm, returning the NON-NEGATIVE result. `None` iff the
/// magnitude is `i64::MIN` (its abs overflows i64) — the caller maps that to 22003, like PG. The
/// `b == -1` guard avoids the `i64::MIN % -1` overflow (the remainder is always 0).
fn gcd_i64(mut a: i64, mut b: i64) -> Option<i64> {
    while b != 0 {
        let t = if b == -1 { 0 } else { a % b };
        a = b;
        b = t;
    }
    a.checked_abs()
}

/// gcd of two decimals by the Euclidean algorithm over `rem`, result NON-NEGATIVE at scale
/// max(sₐ, s_b) (PG numeric gcd). The values share a fixed scale through the chain, so it reduces
/// to an integer gcd on the coefficients and always terminates. The final pad to the target scale
/// is exact (the value's natural scale never exceeds it).
fn gcd_decimal(a: &Decimal, b: &Decimal) -> Result<Decimal> {
    let target = a.scale().max(b.scale());
    let (mut x, mut y) = (a.clone(), b.clone());
    while !y.is_zero() {
        let r = x.rem(&y)?;
        x = y;
        y = r;
    }
    Ok(x.abs().round_to_scale(target))
}

/// lcm of two i64, NON-NEGATIVE: |a/gcd · b|, with checked arithmetic. `None` on i64 overflow
/// (the product, or the final abs) — the caller maps that (or an out-of-result-type magnitude) to
/// 22003, like PG. lcm(_, 0) = 0 (no division by the gcd, which would be 0).
fn lcm_i64(a: i64, b: i64) -> Option<i64> {
    if a == 0 || b == 0 {
        return Some(0);
    }
    let g = gcd_i64(a, b)?; // ≥ 1 for nonzero operands
    let prod = (a / g).checked_mul(b)?;
    prod.checked_abs()
}

/// lcm of two decimals, NON-NEGATIVE at scale max(sₐ, s_b): |a/gcd · b| (the a/gcd division is
/// exact). lcm(_, 0) = 0. A magnitude over the decimal value cap traps 22003 via the mul.
fn lcm_decimal(a: &Decimal, b: &Decimal) -> Result<Decimal> {
    let target = a.scale().max(b.scale());
    if a.is_zero() || b.is_zero() {
        return Ok(Decimal::zero(target));
    }
    let g = gcd_decimal(a, b)?;
    let prod = a.div(&g)?.mul(b)?;
    Ok(prod.abs().round_to_scale(target))
}

/// The 2201G raised by width_bucket for a bad count / equal-or-nonfinite bounds.
fn width_bucket_err(detail: &str) -> EngineError {
    EngineError::new(SqlState::InvalidArgumentForWidthBucketFunction, detail)
}

/// The MINIMUM scale that represents `d` exactly — its display scale minus trailing fractional zeros
/// (decimal.md, the shared engine of min_scale/trim_scale). Reduces the scale one step at a time:
/// round_to_scale(t-1) equals the value iff the digit at scale t is zero (otherwise it rounds,
/// changing the value), so the loop stops at the first non-zero fractional digit. Zero → 0.
fn min_scale_of(d: &Decimal) -> u32 {
    if d.is_zero() {
        return 0;
    }
    let mut t = d.scale();
    while t > 0 && d.round_to_scale(t - 1).cmp_value(d) == std::cmp::Ordering::Equal {
        t -= 1;
    }
    t
}

/// width_bucket over numerics (spec/functions/catalog.toml): floor((operand−low)·count/(high−low))
/// + 1, with 0 below low / count+1 at-or-above high, and the reversed (low > high) range. The bucket
/// is an EXACT truncated decimal quotient (all-positive in range, so trunc == floor). Returns the raw
/// index (the caller range-checks it to int4). `count > 0` is checked by the caller.
fn width_bucket_numeric(op: &Decimal, low: &Decimal, high: &Decimal, count: i64) -> Result<i64> {
    use std::cmp::Ordering;
    let cmp_bounds = low.cmp_value(high);
    if cmp_bounds == Ordering::Equal {
        return Err(width_bucket_err("lower bound cannot equal upper bound"));
    }
    let count_dec = Decimal::from_i64(count);
    // floor((hi_num − lo_num)·count / (hi_den − lo_den)), all operands ≥ 0 in range (trunc == floor).
    let bucket =
        |hi_num: &Decimal, lo_num: &Decimal, hi_den: &Decimal, lo_den: &Decimal| -> Result<i64> {
            let num = hi_num.sub(lo_num)?.mul(&count_dec)?;
            let den = hi_den.sub(lo_den)?;
            let q = num.sub(&num.rem(&den)?)?.div(&den)?.round_to_scale(0);
            let b = q
                .to_i64_round()
                .ok_or_else(|| overflow(ScalarType::Int32))?;
            Ok(b.saturating_add(1))
        };
    if cmp_bounds == Ordering::Less {
        // ascending low < high
        if op.cmp_value(low) == Ordering::Less {
            Ok(0)
        } else if op.cmp_value(high) != Ordering::Less {
            Ok(count.saturating_add(1))
        } else {
            bucket(op, low, high, low)
        }
    } else {
        // descending low > high
        if op.cmp_value(low) == Ordering::Greater {
            Ok(0)
        } else if op.cmp_value(high) != Ordering::Greater {
            Ok(count.saturating_add(1))
        } else {
            bucket(low, op, low, high)
        }
    }
}

/// width_bucket over f64 (spec/functions/catalog.toml): the same index in binary64 (a single
/// correctly-rounded chain, so cross-core identical). A NaN operand/bound → 2201G; a non-finite
/// bound → 2201G (the operand may be ±Inf, handled by the comparisons). Returns the raw index.
fn width_bucket_float(op: f64, low: f64, high: f64, count: i64) -> Result<i64> {
    if op.is_nan() || low.is_nan() || high.is_nan() {
        return Err(width_bucket_err(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    if !low.is_finite() || !high.is_finite() {
        return Err(width_bucket_err("lower and upper bounds must be finite"));
    }
    if low == high {
        return Err(width_bucket_err("lower bound cannot equal upper bound"));
    }
    let cf = count as f64;
    let idx = if low < high {
        if op < low {
            0
        } else if op >= high {
            count.saturating_add(1)
        } else {
            (((op - low) / (high - low) * cf).floor() as i64).saturating_add(1)
        }
    } else if op > low {
        0
    } else if op <= high {
        count.saturating_add(1)
    } else {
        (((low - op) / (low - high) * cf).floor() as i64).saturating_add(1)
    };
    Ok(idx)
}

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
        // bind parameter) to i64; a string literal stays text (no bytea context — types.md §6).
        let (rl, lt) = resolve(scope, lhs, Some(ScalarType::Int64), agg, params)?;
        let (rr, rt) = resolve(scope, rhs, Some(ScalarType::Int64), agg, params)?;
        (rl, lt, rr, rt)
    } else if lhs_lit {
        let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
        let (rl, lt) = resolve(scope, lhs, ctx_of(&rt), agg, params)?;
        (rl, lt, rr, rt)
    } else if rhs_lit {
        let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
        let (rr, rt) = resolve(scope, rhs, ctx_of(&lt), agg, params)?;
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
        Expr::Literal(Literal::Int(_))
            | Expr::Literal(Literal::Decimal(_))
            | Expr::Literal(Literal::Text(_))
            | Expr::Param(_)
    )
}

/// The context type a sibling operand offers an adaptable operand. For an integer literal this
/// is the integer width it adopts; for a string literal, `bytea`/`uuid`/`text` (so it can decode
/// the hex/uuid input); a bind parameter additionally adopts a `decimal`/`boolean` sibling (a
/// literal ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a
/// bare NULL offers no context.
fn ctx_of(ty: &ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(*t),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        // A json/jsonb sibling offers its type so a string literal parses as that type.
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null => None,
        // A composite/array/range sibling offers no scalar adaptation context.
        ResolvedType::Composite(_) | ResolvedType::Array(_) | ResolvedType::Range(_) => None,
        // A datetime sibling offers its type so a string literal parses as that datetime.
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        // A date sibling offers its type so a string literal parses as a date.
        ResolvedType::Date => Some(ScalarType::Date),
        // An interval sibling offers its type so a string literal parses as an interval.
        ResolvedType::Interval => Some(ScalarType::Interval),
        // A float sibling offers its width so an integer/decimal literal ADAPTS to a float
        // context (decimal/int → float at the sibling's width — spec/design/float.md §4). A bare
        // string literal does NOT adapt to a float sibling (its Literal::Text arm keeps it text),
        // so widening the mapping is safe.
        ResolvedType::Float(st) => Some(*st),
    }
}

/// Require that an arithmetic operand is numeric (integer or decimal, or NULL); a boolean,
/// text, or bytea operand is a 42804 type error.
/// The result type of a temporal `+`/`-` (spec/design/interval.md §5), or `None` when neither
/// operand is temporal (interval / timestamp / timestamptz) — then arithmetic falls through to
/// the numeric path. `Some(Err)` is a temporal operand in an unsupported combination (42804). A
/// NULL operand adopts the other side's temporal type (so `timestamp ± NULL` types as timestamp
/// and evaluates to NULL).
fn temporal_arith_result(
    op: BinaryOp,
    lt: &ResolvedType,
    rt: &ResolvedType,
) -> Option<Result<ScalarType>> {
    use ResolvedType as R;
    let temporal = |t: &R| matches!(t, R::Interval | R::Timestamp | R::Timestamptz);
    if !temporal(lt) && !temporal(rt) {
        return None;
    }
    let l = if matches!(lt, R::Null) { rt } else { lt };
    let r = if matches!(rt, R::Null) { lt } else { rt };
    use BinaryOp::{Add, Sub};
    let st = match (op, l, r) {
        (Add | Sub, R::Interval, R::Interval) => ScalarType::Interval,
        (Add, R::Timestamp, R::Interval)
        | (Add, R::Interval, R::Timestamp)
        | (Sub, R::Timestamp, R::Interval) => ScalarType::Timestamp,
        (Add, R::Timestamptz, R::Interval)
        | (Add, R::Interval, R::Timestamptz)
        | (Sub, R::Timestamptz, R::Interval) => ScalarType::Timestamptz,
        (Sub, R::Timestamp, R::Timestamp) | (Sub, R::Timestamptz, R::Timestamptz) => {
            ScalarType::Interval
        }
        _ => {
            return Some(Err(type_error(
                "unsupported operand types for temporal arithmetic",
            )));
        }
    };
    Some(Ok(st))
}

/// The result type of a `date` arithmetic operator (spec/design/date.md §6): `date ± integer →
/// date`, `integer + date → date` (Add commutes; an integer of any width — the family matches
/// i16/i32/i64), `date − date → i32` (the count of days between, PG's int4), and `date ± interval
/// → timestamp` (the date widens to midnight, then the timestamp ± interval calendar shift — PG:
/// `date + interval` is a `timestamp`, not a date). `interval + date` commutes (Add only); there
/// is no `integer − date` nor `interval − date`. Any other combination involving a date is a
/// 42804 (PG reports "operator does not exist"; jed uses the datatype-mismatch code its other
/// arithmetic type errors use). A bare untyped NULL partner is NOT adopted — `date ± NULL` is a
/// 42804 (PG rejects the ambiguous form too); a typed NULL keeps its family and resolves here.
fn date_arith_result(op: BinaryOp, lt: &ResolvedType, rt: &ResolvedType) -> Result<ScalarType> {
    use BinaryOp::{Add, Sub};
    use ResolvedType as R;
    let st = match (op, lt, rt) {
        (Add, R::Date, R::Int(_)) | (Add, R::Int(_), R::Date) | (Sub, R::Date, R::Int(_)) => {
            ScalarType::Date
        }
        (Sub, R::Date, R::Date) => ScalarType::Int32,
        (Add, R::Date, R::Interval) | (Add, R::Interval, R::Date) | (Sub, R::Date, R::Interval) => {
            ScalarType::Timestamp
        }
        _ => return Err(type_error("unsupported operand types for date arithmetic")),
    };
    Ok(st)
}

/// The result type of an interval `×÷` number (spec/design/interval.md §5): `interval * number`,
/// `number * interval` (commute), `interval / number` → interval. `None` when no interval is
/// involved (or the op is not `*`/`/`). A NULL operand counts as a numeric partner (propagates).
/// `number / interval` and `interval × interval` return `None` here and fall to the ±-only
/// temporal rule, which reports the 42804.
fn interval_scale_result(
    op: BinaryOp,
    lt: &ResolvedType,
    rt: &ResolvedType,
) -> Option<Result<ScalarType>> {
    use ResolvedType as R;
    let l_iv = matches!(lt, R::Interval);
    let r_iv = matches!(rt, R::Interval);
    if !l_iv && !r_iv {
        return None;
    }
    let numeric = |t: &R| matches!(t, R::Int(_) | R::Decimal | R::Null);
    match op {
        BinaryOp::Mul if (l_iv && numeric(rt)) || (r_iv && numeric(lt)) => {
            Some(Ok(ScalarType::Interval))
        }
        BinaryOp::Div if l_iv && numeric(rt) => Some(Ok(ScalarType::Interval)),
        _ => None,
    }
}

/// A numeric factor value as an exact fraction `(num, den)` (`den > 0`): an integer is `(n, 1)`;
/// a decimal is parsed from its canonical string (interval.rs). Used by the interval `×÷` cascade.
fn factor_to_fraction(v: &Value) -> Result<(i128, i128)> {
    match v {
        Value::Int(n) => Ok((*n as i128, 1)),
        Value::Decimal(d) => crate::interval::parse_factor_decimal(&d.render()),
        _ => unreachable!("resolver guarantees a numeric interval-scale factor"),
    }
}

fn require_numeric_operand(ty: &ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null => Ok(()),
        // Float reaches here only as the NON-float side of a mixed pair (a pure float × float pair
        // is routed before this) — int/decimal × float is a 42804, the strict island (float.md §6).
        ResolvedType::Bool
        | ResolvedType::Text
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz
        | ResolvedType::Date
        | ResolvedType::Interval
        | ResolvedType::Float(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => {
            Err(type_error("arithmetic operators require numeric operands"))
        }
    }
}

/// Require that a comparison operand pair is comparable (spec/types/compare.toml): both
/// numeric (integer and/or decimal — the integer promotes to decimal), both text, both
/// boolean, or both bytea (NULL counts as any). A cross-family pair (numeric/text,
/// boolean/non-boolean, bytea/non-bytea, …) is a 42804 type error — comparison is overloaded
/// across these families but never compares across them.
fn classify_comparable(lt: &ResolvedType, rt: &ResolvedType) -> Result<()> {
    use ResolvedType::{
        Array, Bool, Bytea, Composite, Date, Decimal, Float, Int, Interval, Json, JsonPath, Jsonb,
        Null, Range, Text, Timestamp, Timestamptz, Uuid,
    };
    match (lt, rt) {
        // json is NOT comparable: PostgreSQL ships no btree/hash operator class for `json`, so jed
        // matches it (spec/design/json.md §5). ANY json comparison — even json × json, json × jsonb,
        // or json × a bare NULL — is `42883` (operator does not exist), distinct from the
        // cross-family `42804` other types use. Must precede the jsonb arms so json × jsonb is 42883.
        (Json, _) | (_, Json) => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: json is not comparable",
        )),
        // jsonpath is likewise NOT comparable (PG ships no opclass — jsonpath.md §1): every
        // comparison is `42883`.
        (JsonPath, _) | (_, JsonPath) => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: jsonpath is not comparable",
        )),
        // jsonb IS comparable — PostgreSQL's total btree order (spec/design/json.md §5) — but only
        // with another jsonb (or a bare NULL). jsonb vs any other family is `42804` (jed's
        // cross-family convention, like uuid/bytea/range; a documented divergence from PG's 42883).
        (Jsonb, Jsonb) | (Jsonb, Null) | (Null, Jsonb) => Ok(()),
        (Jsonb, _) | (_, Jsonb) => Err(type_error(
            "cannot compare a jsonb value with a value of a different type",
        )),
        // Range comparison is the PG `range_cmp` total order (spec/design/ranges.md §6). Two ranges
        // are comparable iff they are over the **same element type** — `i32range × i32range` only,
        // never `i32range × i64range` or `i32range × i32` (no implicit cross-element range
        // comparison this slice; stricter than the int↔bigint scalar case, so the element
        // `ResolvedType`s must be *equal*, not merely comparable). A bare NULL is always comparable.
        (Range(_), Null) | (Null, Range(_)) => Ok(()),
        (Range(a), Range(b)) if a == b => Ok(()),
        (Range(_), Range(_)) => Err(type_error(
            "cannot compare ranges of different element types",
        )),
        (Range(_), _) | (_, Range(_)) => Err(type_error(
            "cannot compare a range value with a value of a different type",
        )),
        // Array comparison is element-wise (spec/design/array.md §5): two arrays are comparable iff
        // their element types are comparable (recursively). A bare NULL is always comparable; an
        // array vs any non-array is 42804.
        (Array(_), Null) | (Null, Array(_)) => Ok(()),
        (Array(a), Array(b)) => classify_comparable(a, b),
        (Array(_), _) | (_, Array(_)) => Err(type_error(
            "cannot compare an array value with a value of a different type",
        )),
        // Composite comparison is element-wise row comparison (spec/design/composite.md §5): two
        // composites are comparable iff they have the SAME field count and each corresponding
        // field pair is itself comparable (recursively — a nested composite recurses here, an
        // anonymous `ROW(…)` compares against a same-shape named type). A bare NULL is always
        // comparable (the comparison is unknown). A composite vs any non-composite, or a row-size
        // mismatch, or an incomparable field pair, is 42804.
        (Composite(_), Null) | (Null, Composite(_)) => Ok(()),
        (Composite(a), Composite(b)) => {
            if a.fields.len() != b.fields.len() {
                return Err(type_error("cannot compare rows of different sizes"));
            }
            for ((_, fa), (_, fb)) in a.fields.iter().zip(b.fields.iter()) {
                classify_comparable(fa, fb)?;
            }
            Ok(())
        }
        (Composite(_), _) | (_, Composite(_)) => Err(type_error(
            "cannot compare a composite value with a value of a different type",
        )),
        // Float is a STRICT ISLAND (spec/design/float.md §3/§6): comparable only float × float
        // (either width — a mixed-width pair promotes to f64 first, compare.toml `max-rank`)
        // or with a bare NULL. Float vs ANY other family (int/decimal included) is 42804 — jed
        // requires an explicit cast, a documented divergence from PG which promotes to float8.
        (Float(_), Float(_)) => Ok(()),
        (Float(_), Null) | (Null, Float(_)) => Ok(()),
        (Float(_), _) | (_, Float(_)) => Err(type_error(
            "cannot compare a float value with a value of a different type",
        )),
        // interval compares only within its own family (or with a bare NULL), by the canonical
        // span (spec/design/interval.md §2). interval vs any other family is a 42804.
        (Interval, Interval) => Ok(()),
        (Interval, Null) | (Null, Interval) => Ok(()),
        (Interval, _) | (_, Interval) => Err(type_error(
            "cannot compare an interval value with a value of a different type",
        )),
        // timestamp / timestamptz compare only within their own family (or with a bare NULL).
        // A mixed timestamp × timestamptz pair — or a datetime vs any other family — would need
        // a zone, so it is a 42804 type error (spec/design/timestamp.md §5).
        (Timestamp, Timestamp) | (Timestamptz, Timestamptz) => Ok(()),
        (Timestamp, Null) | (Null, Timestamp) | (Timestamptz, Null) | (Null, Timestamptz) => Ok(()),
        (Timestamp, _) | (_, Timestamp) | (Timestamptz, _) | (_, Timestamptz) => Err(type_error(
            "cannot compare a timestamp value with a value of a different type",
        )),
        // date compares only within its own family (or with a bare NULL), by the i32 day count
        // (spec/design/date.md §4). date vs any other family — including timestamp, which would
        // need a cast (a documented divergence from PG) — is a 42804.
        (Date, Date) => Ok(()),
        (Date, Null) | (Null, Date) => Ok(()),
        (Date, _) | (_, Date) => Err(type_error(
            "cannot compare a date value with a value of a different type",
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
fn int_type(ty: &ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(*t),
        _ => None,
    }
}

/// Wrap a `f32`-typed operand in an implicit `CAST(... AS f64)` so a mixed-width float
/// pair (compare or arith) computes at one width (spec/design/float.md §2/§5). A f64 or
/// non-float operand is returned unchanged; the caller decides when widening is needed.
fn widen_float_to_f64(node: RExpr, ty: &ResolvedType) -> RExpr {
    if matches!(ty, ResolvedType::Float(ScalarType::Float32)) {
        RExpr::Cast {
            inner: Box::new(node),
            target: ScalarType::Float64,
            typmod: None,
            varchar_len: None,
        }
    } else {
        node
    }
}

/// Resolve a float arithmetic pair to `(lhs, rhs, result_width)` with mixed widths promoted to
/// f64 (spec/design/float.md §5). Returns `None` when the pair is NOT a pure float pair (one
/// side is a non-float, non-NULL family) — the caller then raises the strict-island 42804. A
/// `float × NULL` pair adopts the float side's width (the NULL propagates at eval).
fn promote_float_arith(
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
) -> Option<(RExpr, RExpr, ScalarType)> {
    use ResolvedType::{Float, Null};
    let width = match (&lt, &rt) {
        (Float(a), Float(b)) => {
            if a.rank() >= b.rank() {
                *a
            } else {
                *b
            }
        }
        (Float(a), Null) | (Null, Float(a)) => *a,
        _ => return None,
    };
    // Promote a f32 operand to the common width when the result is f64.
    let (rl, rr) = if width == ScalarType::Float64 {
        (widen_float_to_f64(rl, &lt), widen_float_to_f64(rr, &rt))
    } else {
        (rl, rr)
    };
    Some((rl, rr, width))
}

/// The promotion-tower result type of two arithmetic operands: the higher-ranked
/// integer type, or i64 when both are untyped NULLs.
fn promote(a: &ResolvedType, b: &ResolvedType) -> ScalarType {
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
fn require_text_or_null(ty: &ResolvedType) -> Result<()> {
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
/// Unify the element types of an `ARRAY[…]` constructor into one element type (spec/design/array.md
/// §1). All-NULL → text (the PG unknown rule). All integer → the widest via the promotion tower (no
/// runtime coercion — every integer is an i64 value). Otherwise every element must be the SAME
/// family — a cross-family mix (including int + decimal) is a documented `42804` narrowing this
/// slice (the representation-changing coercion is deferred with `numeric(p,s)[]`).
fn unify_array_element_types(types: &[ResolvedType]) -> Result<ResolvedType> {
    let non_null: Vec<&ResolvedType> = types.iter().filter(|t| **t != ResolvedType::Null).collect();
    let Some(&first) = non_null.first() else {
        return Ok(ResolvedType::Text);
    };
    if non_null.iter().all(|t| matches!(t, ResolvedType::Int(_))) {
        let mut acc = first.clone();
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(&acc, t));
        }
        return Ok(acc);
    }
    for t in &non_null[1..] {
        if std::mem::discriminant(*t) != std::mem::discriminant(first) {
            return Err(type_error(
                "array elements must all be of the same type".to_string(),
            ));
        }
    }
    Ok(first.clone())
}

/// A `2202E` array-subscript error (spec/design/array.md §11).
fn array_subscript_err(detail: &str) -> EngineError {
    EngineError::new(SqlState::ArraySubscriptError, detail.to_string())
}

/// Stack the evaluated elements of a **nested** `ARRAY[…]` constructor into a value of one higher
/// dimension (spec/design/array.md §4). The resolver guarantees every item resolved to an array; a
/// NULL sub-array or a sub-array of differing shape is a `2202E` ("multidimensional arrays must
/// have array expressions with matching dimensions"). Stacking empty sub-arrays yields the empty
/// array (PG: `ARRAY['{}'::int[]]` → `{}`).
fn build_nested_array(subs: Vec<Value>) -> Result<Value> {
    const MISMATCH: &str =
        "multidimensional arrays must have array expressions with matching dimensions";
    let mut arrs = Vec::with_capacity(subs.len());
    for s in subs {
        match s {
            Value::Array(a) => arrs.push(a),
            Value::Null => return Err(array_subscript_err(MISMATCH)),
            other => unreachable!("nested array constructor over a non-array: {other:?}"),
        }
    }
    let dims0 = arrs[0].dims.clone();
    let lbounds0 = arrs[0].lbounds.clone();
    for a in &arrs[1..] {
        if a.dims != dims0 || a.lbounds != lbounds0 {
            return Err(array_subscript_err(MISMATCH));
        }
    }
    if dims0.is_empty() {
        return Ok(Value::Array(ArrayVal::empty())); // all sub-arrays empty → empty array
    }
    let mut dims = vec![arrs.len()];
    dims.extend(dims0);
    let mut lbounds = vec![1i32];
    lbounds.extend(lbounds0);
    let mut elements = Vec::new();
    for a in arrs {
        elements.extend(a.elements);
    }
    Ok(Value::Array(ArrayVal {
        dims,
        lbounds,
        elements,
    }))
}

/// An evaluated slice bound: omitted (defer to the array's own bound), a NULL bound, or an integer.
#[derive(Clone, Copy)]
pub(crate) enum Bound {
    Omitted,
    Null,
    Val(i64),
}

impl Bound {
    /// The bound as `Option<i64>` (omitted → `None`, to be defaulted by the slice); `Null` must be
    /// handled by the caller before this is called.
    fn value(self) -> Option<i64> {
        match self {
            Bound::Val(i) => Some(i),
            _ => None,
        }
    }
}

/// Count the NULL (when `want_nulls`) or non-NULL values in `vals` — the shared kernel of
/// num_nulls / num_nonnulls (spec/design/array-functions.md §12), over either the spread arguments
/// or a VARIADIC array's flattened elements.
fn count_nulls<'a>(vals: impl Iterator<Item = &'a Value>, want_nulls: bool) -> usize {
    vals.filter(|v| matches!(v, Value::Null) == want_nulls)
        .count()
}

/// Evaluate an array function over its already-evaluated argument values
/// (spec/design/array-functions.md §3). The introspectors `propagate` NULL and return NULL for an
/// out-of-shape request; the builders are non-strict (a NULL array argument is the identity/empty,
/// NOT a propagated NULL). The resolver guarantees the array operand is an array or NULL, so the
/// `_` arms are genuinely unreachable.
fn eval_array_func(func: &ArrayFunc, vals: &[Value]) -> Result<Value> {
    match func {
        ArrayFunc::ArrayNdims => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) if a.ndim() == 0 => Ok(Value::Null), // empty array → NULL (PG)
            Value::Array(a) => Ok(Value::Int(a.ndim() as i64)),
            _ => unreachable!("array_ndims: array operand"),
        },
        ArrayFunc::Cardinality => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) => Ok(Value::Int(a.elements.len() as i64)), // 0 for empty (NOT NULL)
            _ => unreachable!("cardinality: array operand"),
        },
        ArrayFunc::ArrayDims => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) if a.ndim() == 0 => Ok(Value::Null),
            Value::Array(a) => Ok(Value::Text(array_dims_text(a))),
            _ => unreachable!("array_dims: array operand"),
        },
        // array_to_json(anyarray) → the array's compact JSON image (the to_jsonb node kernel). STRICT;
        // a multidimensional array propagates the to_jsonb 0A000.
        ArrayFunc::ArrayToJson => match &vals[0] {
            Value::Null => Ok(Value::Null),
            _ => Ok(Value::Json(json::json_compact_out(&value_to_node(
                &vals[0],
            )?))),
        },
        // array_length / array_lower / array_upper (anyarray, dim): propagate either NULL arg,
        // and return NULL for an empty array or an out-of-range dimension.
        ArrayFunc::ArrayLength | ArrayFunc::ArrayLower | ArrayFunc::ArrayUpper => {
            let a = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Array(a) => a,
                _ => unreachable!("array_length/lower/upper: array operand"),
            };
            let dim = match &vals[1] {
                Value::Null => return Ok(Value::Null),
                Value::Int(d) => *d,
                _ => unreachable!("the dimension argument is the integer family"),
            };
            if a.ndim() == 0 || dim < 1 || dim > a.ndim() as i64 {
                return Ok(Value::Null);
            }
            let d = (dim - 1) as usize;
            let v = match func {
                ArrayFunc::ArrayLength => a.dims[d] as i64,
                ArrayFunc::ArrayLower => a.lbounds[d] as i64,
                ArrayFunc::ArrayUpper => a.ubound(d) as i64,
                _ => unreachable!(),
            };
            Ok(Value::Int(v))
        }
        ArrayFunc::ArrayAppend => array_extend(&vals[0], &vals[1], true),
        ArrayFunc::ArrayPrepend => array_extend(&vals[1], &vals[0], false),
        ArrayFunc::ArrayCat => array_cat_values(&vals[0], &vals[1]),
        ArrayFunc::ArrayRemove => array_remove_value(&vals[0], &vals[1]),
        ArrayFunc::ArrayReplace => array_replace_value(&vals[0], &vals[1], &vals[2]),
        ArrayFunc::ArrayPosition => array_position_value(&vals[0], &vals[1], vals.get(2)),
        ArrayFunc::ArrayPositions => array_positions_value(&vals[0], &vals[1]),
        ArrayFunc::Contains => array_contains_value(&vals[0], &vals[1]),
        ArrayFunc::ContainedBy => array_contains_value(&vals[1], &vals[0]),
        ArrayFunc::Overlaps => array_overlaps_value(&vals[0], &vals[1]),
    }
}

/// Evaluate a range accessor (spec/design/range-functions.md §1). STRICT: a NULL range → NULL.
/// `lower`/`upper` yield the bound value (NULL when empty or unbounded on that side); the `_inc`/
/// `_inf` readers + `isempty` yield boolean. For the empty range every reader but `isempty` is
/// false/NULL; for an infinite bound the `_inf` reader is true and the `_inc` reader false.
fn eval_range_func(func: &RangeFunc, vals: &[Value]) -> Result<Value> {
    let rv = match &vals[0] {
        Value::Null => return Ok(Value::Null),
        Value::Range(rv) => rv,
        _ => unreachable!("range accessor: range operand"),
    };
    Ok(match func {
        RangeFunc::Lower => match (rv.empty, &rv.lower) {
            (false, Some(v)) => (**v).clone(),
            _ => Value::Null,
        },
        RangeFunc::Upper => match (rv.empty, &rv.upper) {
            (false, Some(v)) => (**v).clone(),
            _ => Value::Null,
        },
        RangeFunc::IsEmpty => Value::Bool(rv.empty),
        // For the empty range both inclusivity flags are false by the canonical invariant, so reading
        // them directly already yields PG's `false`; an infinite bound likewise stores `_inc = false`.
        RangeFunc::LowerInc => Value::Bool(rv.lower_inc),
        RangeFunc::UpperInc => Value::Bool(rv.upper_inc),
        // The empty range is NOT infinite on either side (PG): guard before reading the bound.
        RangeFunc::LowerInf => Value::Bool(!rv.empty && rv.lower.is_none()),
        RangeFunc::UpperInf => Value::Bool(!rv.empty && rv.upper.is_none()),
    })
}

/// Build a range value from a constructor call's evaluated arguments (range-functions.md §2). `vals`
/// is `[lo, hi]` or `[lo, hi, bounds]`. Each bound is coerced to the element `elem` assignment-style
/// (a NULL bound → an infinite bound; an integer range-checks 22003; an int→decimal / text→temporal
/// adapts), the bounds flags are read (default `[)`; a NULL 3-arg flags → 22000; an invalid flags
/// string → 42601), and `finalize` produces the canonical value (order-check 22000, canonicalize,
/// empty-normalize).
fn eval_range_ctor(elem: ScalarType, vals: &[Value]) -> Result<Value> {
    let desc =
        crate::range::range_for_element(elem).expect("a range constructor's elem has a range");
    let lower = coerce_range_bound(vals[0].clone(), elem)?;
    let upper = coerce_range_bound(vals[1].clone(), elem)?;
    let (lower_inc, upper_inc) = match vals.get(2) {
        None => (true, false), // 2-arg form defaults to `[)`
        Some(Value::Null) => {
            return Err(EngineError::new(
                SqlState::DataException,
                "range constructor flags argument must not be null".to_string(),
            ));
        }
        Some(Value::Text(s)) => crate::range::parse_bound_flags(s)?,
        Some(_) => unreachable!("resolver restricts the range bounds flags to text"),
    };
    Ok(Value::Range(crate::range::finalize(
        desc, lower, upper, lower_inc, upper_inc,
    )?))
}

/// Coerce one constructor bound value to the range element `elem`, returning `None` for a NULL bound
/// (an infinite bound). Reuses [`store_value`] (the INSERT/UPDATE assignment coercion): an integer
/// range-checks into the element (22003), an int→decimal widens, a text→temporal parses, and a
/// non-assignable value is 42804 (the resolver already screened the common 42883 cases).
fn coerce_range_bound(v: Value, elem: ScalarType) -> Result<Option<Value>> {
    match store_value(v, elem, None, None, false, "range bound")? {
        Value::Null => Ok(None),
        other => Ok(Some(other)),
    }
}

/// Evaluate a range boolean operator (range-functions.md §3, RF3) over two already-evaluated operand
/// values. STRICT: a NULL operand → NULL. For the range-against-range operators both operands are
/// ranges; for the element overloads (`ContainsElem`/`ElemContainedBy`) the non-range operand is
/// coerced to the range's element type `elem` (assignment-style, matching the resolver's hint). The
/// boolean kernels live in `range.rs`.
fn eval_range_op(op: RangeOp, l: &Value, r: &Value, elem: ScalarType) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let result = match op {
        // `range @> element`: l is the range, r the element (coerced to the range's element type).
        RangeOp::ContainsElem => {
            let e = store_value(r.clone(), elem, None, None, false, "range element")?;
            crate::range::range_contains_elem(expect_range(l), &e)
        }
        // `element <@ range`: l is the element, r the range.
        RangeOp::ElemContainedBy => {
            let e = store_value(l.clone(), elem, None, None, false, "range element")?;
            crate::range::range_contains_elem(expect_range(r), &e)
        }
        _ => {
            let (a, b) = (expect_range(l), expect_range(r));
            match op {
                RangeOp::Contains => crate::range::range_contains(a, b),
                RangeOp::ContainedBy => crate::range::range_contains(b, a),
                RangeOp::Overlaps => crate::range::range_overlaps(a, b),
                RangeOp::Before => crate::range::range_before(a, b),
                RangeOp::After => crate::range::range_after(a, b),
                RangeOp::Overleft => crate::range::range_overleft(a, b),
                RangeOp::Overright => crate::range::range_overright(a, b),
                RangeOp::Adjacent => crate::range::range_adjacent(a, b),
                RangeOp::ContainsElem | RangeOp::ElemContainedBy => {
                    unreachable!("element overloads handled above")
                }
            }
        }
    };
    Ok(Value::Bool(result))
}

/// Evaluate a range SET operator (range-functions.md §4, RF4) over two already-evaluated operands.
/// STRICT: a NULL operand → NULL. Dispatches to the `range.rs` kernels; `+` (`Union`) and `-`
/// (`Difference`) raise 22000 on a non-contiguous result, `*` (`Intersect`) and `range_merge`
/// (`Merge`) never error.
fn eval_range_set_op(op: RangeSetOp, l: &Value, r: &Value) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let (a, b) = (expect_range(l), expect_range(r));
    let rv = match op {
        RangeSetOp::Union => crate::range::range_union(a, b, true)?,
        RangeSetOp::Merge => crate::range::range_union(a, b, false)?,
        RangeSetOp::Intersect => crate::range::range_intersect(a, b),
        RangeSetOp::Difference => crate::range::range_minus(a, b)?,
    };
    Ok(Value::Range(rv))
}

/// Extract the [`RangeVal`] from a value the resolver guaranteed is a (non-NULL) range operand.
fn expect_range(v: &Value) -> &RangeVal {
    match v {
        Value::Range(rv) => rv,
        _ => unreachable!("the range-operator resolver guarantees a range operand here"),
    }
}

/// STRICT element equality for the containment/overlap operators (array-functions.md §10): a NULL
/// element equals NOTHING — including another NULL — the deliberate inverse of `not_distinct` (§5
/// #10). For two non-NULL values it is jed's total element comparator (`value_cmp == Equal`), which
/// for jed's element types agrees with PostgreSQL's per-type btree equality.
fn strict_elem_eq(a: &Value, b: &Value) -> bool {
    !matches!(a, Value::Null)
        && !matches!(b, Value::Null)
        && value_cmp(a, b) == std::cmp::Ordering::Equal
}

/// `a @> b` (array-functions.md §10): does `a` CONTAIN `b` — is every element of `b` present in `a`
/// under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
/// whole-array operand → NULL. The empty array is contained by anything (`a @> {}` is true).
fn array_contains_value(a: &Value, b: &Value) -> Result<Value> {
    let (ca, cb) = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Array(ca), Value::Array(cb)) => (ca, cb),
        _ => unreachable!("array containment: array operands"),
    };
    let contained = cb
        .elements
        .iter()
        .all(|eb| ca.elements.iter().any(|ea| strict_elem_eq(ea, eb)));
    Ok(Value::Bool(contained))
}

/// `a && b` (array-functions.md §10): do `a` and `b` OVERLAP — share at least one element under
/// STRICT equality, over the flattened element multiset (any dimensionality)? A NULL whole-array
/// operand → NULL. The empty array overlaps nothing.
fn array_overlaps_value(a: &Value, b: &Value) -> Result<Value> {
    let (ca, cb) = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Array(ca), Value::Array(cb)) => (ca, cb),
        _ => unreachable!("array overlap: array operands"),
    };
    let overlaps = ca
        .elements
        .iter()
        .any(|ea| cb.elements.iter().any(|eb| strict_elem_eq(ea, eb)));
    Ok(Value::Bool(overlaps))
}

/// IS NOT DISTINCT FROM at the value level (array-functions.md §5 #10): jed's total element
/// comparator (the array-element / btree equality), so `NULL` equals `NULL` and a non-NULL never
/// equals `NULL`. For jed's element types this agrees with PostgreSQL's per-type btree equality.
fn not_distinct(a: &Value, b: &Value) -> bool {
    value_cmp(a, b) == std::cmp::Ordering::Equal
}

/// array_remove(a, e) (array-functions.md §8): drop every element NOT DISTINCT FROM `e`. NULL array
/// → NULL; **1-D/empty only** (a multidimensional array is 0A000); the lower bound is preserved and
/// an all-removed result is the empty array `{}`.
fn array_remove_value(arr: &Value, elem: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_remove: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "removing elements from multidimensional arrays is not supported",
        ));
    }
    let kept: Vec<Value> = a
        .elements
        .iter()
        .filter(|e| !not_distinct(e, elem))
        .cloned()
        .collect();
    if kept.is_empty() {
        return Ok(Value::Array(ArrayVal::empty()));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    Ok(Value::Array(ArrayVal {
        dims: vec![kept.len()],
        lbounds: vec![lb],
        elements: kept,
    }))
}

/// array_replace(a, from, to) (array-functions.md §8): substitute every element NOT DISTINCT FROM
/// `from` with `to`. Works on **any** dimensionality — the shape (dims/lbounds) is preserved and
/// only matching element values change. NULL array → NULL.
fn array_replace_value(arr: &Value, from: &Value, to: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_replace: array operand"),
    };
    let elements = a
        .elements
        .iter()
        .map(|e| {
            if not_distinct(e, from) {
                to.clone()
            } else {
                e.clone()
            }
        })
        .collect();
    Ok(Value::Array(ArrayVal {
        dims: a.dims.clone(),
        lbounds: a.lbounds.clone(),
        elements,
    }))
}

/// array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the array's lower-bound
/// space) of the first element NOT DISTINCT FROM `e`, NULL if absent. **1-D/empty only** (a
/// multidimensional array is 0A000); the optional `start` is a subscript to begin the scan at, and a
/// NULL `start` is 22004.
fn array_position_value(arr: &Value, elem: &Value, start: Option<&Value>) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_position: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    // The scan's 0-based start offset into `elements`: by default the array's lower bound; an
    // explicit `start` is a SUBSCRIPT, so the offset is `start - lb` (clamped to >= 0).
    let begin = match start {
        None => 0usize,
        Some(Value::Null) => {
            return Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "initial position must not be null",
            ));
        }
        Some(Value::Int(s)) => (s - lb as i64).max(0) as usize,
        _ => unreachable!("array_position: start is the integer family"),
    };
    for (i, e) in a.elements.iter().enumerate().skip(begin) {
        if not_distinct(e, elem) {
            return Ok(Value::Int(lb as i64 + i as i64));
        }
    }
    Ok(Value::Null)
}

/// array_positions(a, e) (array-functions.md §8): the i32[] of every match's subscript (in the
/// array's lower-bound space), the empty array `{}` if none. NULL array → NULL; **1-D/empty only**
/// (a multidimensional array is 0A000).
fn array_positions_value(arr: &Value, elem: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_positions: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    let positions: Vec<Value> = a
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| not_distinct(e, elem))
        .map(|(i, _)| Value::Int(lb as i64 + i as i64))
        .collect();
    Ok(Value::Array(ArrayVal::one_dim(positions)))
}

/// The `array_dims` text form `[l1:u1][l2:u2]…` (no trailing `=`, unlike `array_out`'s prefix —
/// array-functions.md §3.1).
fn array_dims_text(a: &ArrayVal) -> String {
    let mut s = String::new();
    for d in 0..a.ndim() {
        s.push('[');
        s.push_str(&a.lbounds[d].to_string());
        s.push(':');
        s.push_str(&a.ubound(d).to_string());
        s.push(']');
    }
    s
}

/// array_append (`append=true`) / array_prepend (array-functions.md §3.2). The array side is
/// non-strict: a NULL or empty array yields the 1-D singleton `{elem}` (lower bound 1). A 1-D array
/// grows by one element, preserving its lower bound; a multidimensional array is `22000`.
fn array_extend(arr: &Value, elem: &Value, append: bool) -> Result<Value> {
    let av = match arr {
        Value::Null => None,
        Value::Array(a) => Some(a),
        _ => unreachable!("array_append/prepend: array operand"),
    };
    match av {
        None => Ok(Value::Array(ArrayVal::one_dim(vec![elem.clone()]))),
        Some(a) if a.ndim() == 0 => Ok(Value::Array(ArrayVal::one_dim(vec![elem.clone()]))),
        Some(a) if a.ndim() == 1 => {
            let mut elements = a.elements.clone();
            if append {
                elements.push(elem.clone());
            } else {
                elements.insert(0, elem.clone());
            }
            Ok(Value::Array(ArrayVal {
                dims: vec![a.dims[0] + 1],
                lbounds: a.lbounds.clone(),
                elements,
            }))
        }
        Some(_) => Err(EngineError::new(
            SqlState::DataException,
            "argument must be empty or one-dimensional array",
        )),
    }
}

/// array_cat (array-functions.md §3.2): identity-aware concatenation along the outer dimension.
/// NULL/empty is the identity (both NULL → NULL). Same dimensionality concatenates if the inner
/// dims match; an off-by-one dimensionality appends/prepends the lower one as an outer slice; any
/// other pairing — or an inner-dim mismatch — is `2202E`. The flattened element list is always
/// `a ++ b` (row-major, outer-first); the result lower bounds come from the higher-dim operand.
fn array_cat_values(a: &Value, b: &Value) -> Result<Value> {
    match (a, b) {
        (Value::Null, Value::Null) => return Ok(Value::Null),
        (Value::Null, _) => return Ok(b.clone()),
        (_, Value::Null) => return Ok(a.clone()),
        _ => {}
    }
    let av = match a {
        Value::Array(x) => x,
        _ => unreachable!("array_cat: array operand"),
    };
    let bv = match b {
        Value::Array(x) => x,
        _ => unreachable!("array_cat: array operand"),
    };
    if av.ndim() == 0 {
        return Ok(b.clone());
    }
    if bv.ndim() == 0 {
        return Ok(a.clone());
    }
    let mismatch = || {
        EngineError::new(
            SqlState::ArraySubscriptError,
            "cannot concatenate incompatible arrays",
        )
    };
    let mut elements = av.elements.clone();
    elements.extend(bv.elements.iter().cloned());
    let (na, nb) = (av.ndim(), bv.ndim());
    if na == nb {
        if av.dims[1..] != bv.dims[1..] {
            return Err(mismatch());
        }
        let mut dims = av.dims.clone();
        dims[0] = av.dims[0] + bv.dims[0];
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: av.lbounds.clone(),
            elements,
        }))
    } else if na == nb + 1 {
        if av.dims[1..] != bv.dims[..] {
            return Err(mismatch());
        }
        let mut dims = av.dims.clone();
        dims[0] = av.dims[0] + 1;
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: av.lbounds.clone(),
            elements,
        }))
    } else if nb == na + 1 {
        if bv.dims[1..] != av.dims[..] {
            return Err(mismatch());
        }
        let mut dims = bv.dims.clone();
        dims[0] = bv.dims[0] + 1;
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: bv.lbounds.clone(),
            elements,
        }))
    } else {
        Err(mismatch())
    }
}

/// Evaluate an array subscript `base[..][..]` (spec/design/array.md §6) — the body of
/// [`RExpr::Subscript`]'s eval arm, kept here so its locals stay out of `eval`'s frame. A NULL
/// array or any NULL subscript bound yields NULL; element access returns the element (or NULL),
/// slice access a (renumbered) sub-array.
fn eval_subscript(
    base: &RExpr,
    subscripts: &[RSubscript],
    is_slice: bool,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    let a = match base.eval(row, env, m)? {
        Value::Array(a) => a,
        Value::Null => return Ok(Value::Null),
        other => unreachable!("subscript on a non-array value: {other:?}"),
    };
    if is_slice {
        // Per-dimension (lower, upper); a scalar index `i` becomes `1:i` (PG), an omitted bound
        // defers to the array's own bound. A NULL bound → NULL.
        let mut bounds = Vec::with_capacity(subscripts.len());
        for s in subscripts {
            let b = match s {
                RSubscript::Index(e) => match e.eval(row, env, m)? {
                    Value::Int(i) => (Some(1i64), Some(i)),
                    Value::Null => return Ok(Value::Null),
                    other => unreachable!("non-int array subscript: {other:?}"),
                },
                RSubscript::Slice { lower, upper } => {
                    let lo = eval_opt_bound(lower, row, env, m)?;
                    let hi = eval_opt_bound(upper, row, env, m)?;
                    match (lo, hi) {
                        (Bound::Null, _) | (_, Bound::Null) => return Ok(Value::Null),
                        (lo, hi) => (lo.value(), hi.value()),
                    }
                }
            };
            bounds.push(b);
        }
        Ok(array_get_slice(&a, &bounds))
    } else {
        // Element access: every spec is an index (a slice would have set `is_slice`).
        let mut idxs = Vec::with_capacity(subscripts.len());
        for s in subscripts {
            let RSubscript::Index(e) = s else {
                unreachable!("non-index subscript in element access")
            };
            match e.eval(row, env, m)? {
                Value::Int(i) => idxs.push(i),
                Value::Null => return Ok(Value::Null),
                other => unreachable!("non-int array subscript: {other:?}"),
            }
        }
        Ok(array_get_element(&a, &idxs))
    }
}

/// Evaluate an optional slice-bound expression (spec/design/array.md §6).
fn eval_opt_bound(
    b: &Option<Box<RExpr>>,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Bound> {
    match b {
        None => Ok(Bound::Omitted),
        Some(e) => match e.eval(row, env, m)? {
            Value::Int(i) => Ok(Bound::Val(i)),
            Value::Null => Ok(Bound::Null),
            other => unreachable!("non-int array slice bound: {other:?}"),
        },
    }
}

/// Read a single array element by `idxs` (1-based per dimension, using the value's lower bounds) —
/// spec/design/array.md §6. NULL when the subscript count ≠ `ndim` or any index is out of range.
fn array_get_element(a: &ArrayVal, idxs: &[i64]) -> Value {
    if idxs.len() != a.ndim() || a.elements.is_empty() {
        return Value::Null;
    }
    let mut flat = 0usize;
    let mut stride = 1usize;
    for d in (0..a.ndim()).rev() {
        let lb = a.lbounds[d] as i64;
        let ub = a.ubound(d) as i64;
        if idxs[d] < lb || idxs[d] > ub {
            return Value::Null;
        }
        flat += (idxs[d] - lb) as usize * stride;
        stride *= a.dims[d];
    }
    a.elements[flat].clone()
}

/// Read an array slice (spec/design/array.md §6): per-dimension `(lower, upper)` requested bounds
/// (`None` defers to the value's own bound), clamped to each dimension's `[lb, ub]`. Too many
/// subscripts, an empty source, or any empty clamped dimension yields the empty array; fewer
/// subscripts than `ndim` leave the trailing dimensions at their full range. The result is
/// renumbered to lower bound 1 on every dimension (PG `array_get_slice`).
fn array_get_slice(a: &ArrayVal, bounds: &[(Option<i64>, Option<i64>)]) -> Value {
    let ndim = a.ndim();
    if bounds.len() > ndim || ndim == 0 {
        return Value::Array(ArrayVal::empty());
    }
    let mut new_dims = Vec::with_capacity(ndim);
    let mut starts = Vec::with_capacity(ndim); // source 0-based start per dimension
    for d in 0..ndim {
        let lb = a.lbounds[d] as i64;
        let ub = a.ubound(d) as i64;
        let (req_lo, req_hi) = if d < bounds.len() {
            (bounds[d].0.unwrap_or(lb), bounds[d].1.unwrap_or(ub))
        } else {
            (lb, ub) // a trailing unspecified dimension spans its full range
        };
        let lo = req_lo.max(lb);
        let hi = req_hi.min(ub);
        if lo > hi {
            return Value::Array(ArrayVal::empty()); // any empty dimension → empty slice
        }
        new_dims.push((hi - lo + 1) as usize);
        starts.push((lo - lb) as usize);
    }
    // Row-major strides over the SOURCE array.
    let mut strides = vec![1usize; ndim];
    for d in (0..ndim - 1).rev() {
        strides[d] = strides[d + 1] * a.dims[d + 1];
    }
    let total: usize = new_dims.iter().product();
    let mut elements = Vec::with_capacity(total);
    let mut counter = vec![0usize; ndim];
    for _ in 0..total {
        let mut flat = 0usize;
        for d in 0..ndim {
            flat += (starts[d] + counter[d]) * strides[d];
        }
        elements.push(a.elements[flat].clone());
        for d in (0..ndim).rev() {
            counter[d] += 1;
            if counter[d] < new_dims[d] {
                break;
            }
            counter[d] = 0;
        }
    }
    Value::Array(ArrayVal {
        dims: new_dims,
        lbounds: vec![1i32; ndim],
        elements,
    })
}

fn unify_case_types(arms: &[ResolvedType]) -> Result<ResolvedType> {
    let non_null: Vec<&ResolvedType> = arms.iter().filter(|t| **t != ResolvedType::Null).collect();
    let Some(&first) = non_null.first() else {
        // Every arm is NULL/untyped — PostgreSQL types the CASE as text.
        return Ok(ResolvedType::Text);
    };
    let all_numeric = non_null
        .iter()
        .all(|t| matches!(t, ResolvedType::Int(_) | ResolvedType::Decimal));
    if all_numeric {
        if non_null.iter().any(|t| **t == ResolvedType::Decimal) {
            return Ok(ResolvedType::Decimal);
        }
        // All integer: the widest via the promotion tower (width is unobservable in output —
        // every integer renders under the `I` tag — but the fold keeps the type precise).
        let mut acc = first.clone();
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(&acc, t));
        }
        return Ok(acc);
    }
    // Non-numeric: every arm must be the same family as the first (cross-family is 42804).
    for t in &non_null[1..] {
        if std::mem::discriminant(*t) != std::mem::discriminant(first) {
            return Err(type_error("CASE result types must be compatible"));
        }
    }
    Ok(first.clone())
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
/// Unify two row value types for the SAME VALUES-body column (spec/design/grammar.md §42), the
/// set-operation rule (§25): integer widths widen, `int`+`decimal` → `decimal`, anything + `NULL`
/// keeps the other, and a same-type scalar pair (`text`, `bool`, `bytea`, `uuid`, a `timestamp` /
/// `timestamptz`, an `interval`, a same-width `float`) unifies to itself; any other pair — including
/// a composite or array column across rows (a deferred edge) — is 42804. Enumerated EXPLICITLY (not
/// a generic `a == b`) so all three cores compute byte-identical results (CLAUDE.md §8).
fn unify_values_column(a: &ResolvedType, b: &ResolvedType) -> Result<ResolvedType> {
    use ResolvedType::*;
    Ok(match (a, b) {
        (Null, Null) => Null,
        (Null, x) | (x, Null) => x.clone(),
        (Int(_), Int(_)) => Int(promote(a, b)),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Decimal,
        (Text, Text) => Text,
        (Bool, Bool) => Bool,
        (Bytea, Bytea) => Bytea,
        (Uuid, Uuid) => Uuid,
        (Timestamp, Timestamp) => Timestamp,
        (Timestamptz, Timestamptz) => Timestamptz,
        (Date, Date) => Date,
        (Interval, Interval) => Interval,
        (Float(x), Float(y)) if x == y => Float(*x),
        _ => {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "VALUES types {} and {} cannot be matched",
                    a.type_name(),
                    b.type_name()
                ),
            ));
        }
    })
}

/// The scalar type to note a bind parameter at, given its VALUES column's unified type
/// (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array column
/// has no scalar parameter type, so the parameter stays untyped (42P18 at `finalize`).
fn scalar_for_param_hint(rt: &ResolvedType) -> Option<ScalarType> {
    match rt {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        ResolvedType::Interval => Some(ScalarType::Interval),
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => None,
    }
}

fn unify_setop_column(a: &ResolvedType, b: &ResolvedType, op: SetOpKind) -> Result<ResolvedType> {
    use ResolvedType::*;
    let out = match (a, b) {
        (Null, Null) => Null,
        (Null, x) | (x, Null) => x.clone(),
        (Int(_), Int(_)) => Int(promote(a, b)),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Decimal,
        (Text, Text) => Text,
        (Bool, Bool) => Bool,
        (Bytea, Bytea) => Bytea,
        (Uuid, Uuid) => Uuid,
        (Timestamp, Timestamp) => Timestamp,
        (Timestamptz, Timestamptz) => Timestamptz,
        (Date, Date) => Date,
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
    for (i, (f, t)) in from.iter().zip(to.iter()).enumerate() {
        if matches!(f, ResolvedType::Int(_)) && *t == ResolvedType::Decimal {
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
    // A set-operation ORDER BY accepts only an output column name or ordinal — a general expression key
    // (after the inputs are unified) is 0A000, matching PostgreSQL's "invalid UNION/INTERSECT/EXCEPT
    // ORDER BY clause" (grammar.md §10).
    if key.expr.is_some() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
        ));
    }
    // An output-column ordinal (`... ORDER BY 1`) resolves by position into the output columns; out
    // of [1, ncols] is 42P10 (grammar.md §10). It precedes the name path (an ordinal has no column).
    if let Some(ord) = key.ordinal {
        if ord < 1 || ord > names.len() as i64 {
            return Err(EngineError::new(
                SqlState::InvalidColumnReference,
                format!("ORDER BY position {ord} is not in select list"),
            ));
        }
        return Ok((ord - 1) as usize);
    }
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

fn require_bool(ty: &ResolvedType, msg: &str) -> Result<()> {
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(()),
        ResolvedType::Int(_)
        | ResolvedType::Text
        | ResolvedType::Decimal
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz
        | ResolvedType::Date
        | ResolvedType::Interval
        | ResolvedType::Float(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => Err(type_error(msg)),
    }
}

/// A value assigned to a column must match its family: an integer column takes an
/// integer (or NULL) value; a text column takes a text (or NULL) value; a boolean column
/// takes a boolean (or NULL) value. Any cross-family pair is a 42804 type error. Mirrors
/// the INSERT literal type-check, generalized to expressions.
fn require_assignable(ty: &ResolvedType, col_ty: ScalarType, col: &str) -> Result<()> {
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
    } else if col_ty.is_interval() {
        matches!(ty, ResolvedType::Interval | ResolvedType::Null)
    } else if col_ty.is_date() {
        matches!(ty, ResolvedType::Date | ResolvedType::Null)
    } else if col_ty.is_float() {
        // A float value assigns to an equal-or-wider float column: f32 → f32/f64
        // (implicit widening), f64 → f64 only (f64 → f32 is explicit-CAST only).
        matches!(ty, ResolvedType::Float(st) if st.rank() <= col_ty.rank())
            || matches!(ty, ResolvedType::Null)
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
/// narrowings (a text/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the
/// call site, not here.
/// The maximum `varchar(n)` length — PostgreSQL's `varchar` ceiling (spec/design/types.md §15).
/// Stored on disk as a `u32`, so it fits comfortably.
pub(crate) const MAX_VARCHAR_LEN: u32 = 10485760;

/// Resolve a scalar type name + optional type modifier, returning the type, the decimal typmod
/// (when the type is `decimal`), and the `varchar(n)` max length (when the type is `text` —
/// spec/design/types.md §15). At most one of the two typmods is ever `Some` (they belong to
/// different types). A typmod on any other type is `0A000`.
fn resolve_type_and_typmod(
    name: &str,
    type_mod: &Option<TypeMod>,
) -> Result<(ScalarType, Option<DecimalTypmod>, Option<u32>)> {
    let ty = if let Some(ty) = ScalarType::from_name(name) {
        ty
    } else {
        return Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type does not exist: {name}"),
        ));
    };
    match type_mod {
        None => Ok((ty, None, None)),
        Some(tm) => {
            if ty.is_decimal() {
                Ok((ty, Some(validate_decimal_typmod(tm)?), None))
            } else if ty.is_text() {
                Ok((ty, None, Some(validate_varchar_typmod(tm)?)))
            } else {
                Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "a type modifier is not supported for type {}",
                        ty.canonical_name()
                    ),
                ))
            }
        }
    }
}

/// Validate a `varchar(n)` type modifier: `1 <= n <= 10485760` (PostgreSQL's `varchar` ceiling),
/// else trap 22023 (spec/design/types.md §15). A scale (`varchar(n, m)`) is a syntax error here —
/// `varchar` takes a single length argument.
fn validate_varchar_typmod(tm: &TypeMod) -> Result<u32> {
    if tm.scale.is_some() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "varchar takes exactly one type modifier (a length)".to_string(),
        ));
    }
    let n = tm.precision;
    if n < 1 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "length for type varchar must be at least 1".to_string(),
        ));
    }
    if n > MAX_VARCHAR_LEN as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("length for type varchar cannot exceed {MAX_VARCHAR_LEN}"),
        ));
    }
    Ok(n as u32)
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

/// Coerce a string literal's content to the named scalar `target` at resolve time — the shared
/// engine of the `type 'string'` typed literal and `CAST(<string literal> AS target)` (PG's
/// text→T cast over a literal operand; spec/design/grammar.md §36, types.md §5). Every scalar is
/// reachable: the string-native types parse by their own input (datetime / interval / bytea /
/// uuid), `text` is identity, and the native-syntax types (int / decimal / boolean) are the cast
/// from text admitted only for a literal operand. Errors: `22P02` malformed / `22003` out of
/// range / the type's own parse code. `typmod` (decimal only) re-scales the result.
/// Coerce a composite text literal `'(…)'` to a folded `Value::Composite` — PostgreSQL's
/// `record_in`, the exact inverse of `record_out` (spec/design/composite.md §8). Used by
/// `'(…)'::type` and the `type '(…)'` typed literal. Tokenizes via `value::parse_record_tokens`
/// (a malformed literal or a field-count mismatch is `22P02`), then coerces each present token to
/// its field's type — a scalar via the same string-literal coercion as a typed literal, a NULL
/// token to a NULL, a nested composite field recursively. Folds to a constant `RExpr::Row` of the
/// coerced field nodes (so `eval` rebuilds the `Value::Composite`), statically typed as the named
/// composite. The recursion is sound because every field type was proven to exist at `CREATE TYPE`.
fn coerce_string_to_composite(
    text: &str,
    ct: &CompositeType,
    catalog: &Engine,
) -> Result<(RExpr, ResolvedType)> {
    let malformed = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("malformed record literal: \"{text}\" for type {}", ct.name),
        )
    };
    let tokens = crate::value::parse_record_tokens(text).ok_or_else(malformed)?;
    if tokens.len() != ct.fields.len() {
        return Err(malformed());
    }
    let mut nodes = Vec::with_capacity(tokens.len());
    let mut field_types = Vec::with_capacity(tokens.len());
    for (tok, f) in tokens.into_iter().zip(ct.fields.iter()) {
        match tok {
            // A NULL field: a NULL value, typed by the field's declared type.
            None => {
                nodes.push(RExpr::ConstNull);
                field_types.push((f.name.clone(), resolved_type_of_col(&f.ty, catalog)));
            }
            Some(s) => {
                let (node, ty) = match &f.ty {
                    Type::Composite(r) => {
                        let nested = catalog
                            .composite_type(&r.name)
                            .expect("nested composite type resolved at CREATE TYPE / load");
                        coerce_string_to_composite(&s, nested, catalog)?
                    }
                    Type::Scalar(scalar) => {
                        coerce_string_literal(&s, *scalar, f.decimal, f.varchar_len)?
                    }
                    // An array-typed field (spec/design/array.md §12): the token is an array text
                    // literal, coerced through `array_in` against the element type — the same path a
                    // bare `'{…}'::T[]` cast uses, one level down. Folds to a constant array.
                    Type::Array(elem_ty) => {
                        let elem_col = resolve_col_type(elem_ty, &catalog.read_snap().types);
                        let val = coerce_string_to_array(&s, &elem_col)?;
                        let rt = resolved_type_of_col(&f.ty, catalog);
                        (value_to_rexpr(&val), rt)
                    }
                    // A range field cannot occur: CREATE TYPE rejects a range field (range columns
                    // are not storable yet — R2), so a composite field type is never a range.
                    Type::Range(_) => {
                        unreachable!("a composite range field is rejected at CREATE TYPE (R2)")
                    }
                };
                nodes.push(node);
                field_types.push((f.name.clone(), ty));
            }
        }
    }
    Ok((
        RExpr::Row(nodes),
        ResolvedType::Composite(Box::new(CompositeRType {
            name: Some(ct.name.clone()),
            fields: field_types,
        })),
    ))
}

/// Coerce a range text literal to a constant range expression (`'[1,5)'::i32range` /
/// `i32range '[1,5)'`). Parses the literal, coerces each bound to the element type via the
/// string-literal coercion, then canonicalizes (spec/design/ranges.md §4/§5). Folds to a
/// `ConstRange`. Malformed → `22P02`; `lower>upper` → `22000`; a canonicalize overflow → `22003`.
/// Resolve an UPDATE assignment RHS against a RANGE or ARRAY column (the caller has already
/// rejected composite — 0A000). Mirrors INSERT's value adaptation (ranges.md §5 / array.md §7): a
/// bare string literal adapts to the container via range_in / array_in, a bare NULL is the typed
/// NULL, and any other expression must resolve to the SAME container type (matching element) else
/// 42804. A top-level `$N` parameter is deferred (0A000) — INSERT's param-to-container handling is
/// special and not generalized to the assignment RHS yet.
fn resolve_container_assign(
    scope: &Scope,
    col: &Column,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    let col_rt = resolved_type_of_col(&col.ty, scope.catalog);
    // A bare string literal adapts to the container context (the same string-adapts-to-context
    // rule the cast and INSERT VALUES paths use).
    if let Expr::Literal(Literal::Text(s)) = e {
        match &col.ty {
            Type::Range(elem) => {
                let desc = crate::range::range_for_element(elem.scalar())
                    .expect("a range column's element always has a range type");
                let (node, _) = coerce_string_to_range_expr(s, desc)?;
                return Ok(node);
            }
            Type::Array(elem) => {
                let elem_col = resolve_col_type(elem, &scope.catalog.read_snap().types);
                let val = coerce_string_to_array(s, &elem_col)?;
                return Ok(value_to_rexpr(&val));
            }
            _ => unreachable!("resolve_container_assign is only called for range/array columns"),
        }
    }
    if let Expr::Literal(Literal::Null) = e {
        return Ok(RExpr::ConstNull);
    }
    if let Expr::Param(_) = e {
        let kind = if col.ty.is_array() { "array" } else { "range" };
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!(
                "updating {kind} column {} from a parameter is not supported yet",
                col.name
            ),
        ));
    }
    // For an array column over a SCALAR element, pass the element type as the hint so a bare
    // `ARRAY[1,2]` constructor adapts its literal elements to the column's element type (the same
    // adaptation `col = ARRAY[…]` uses — without it, bare int literals would type as i64 and miss a
    // narrower i32[]/i16[] column). A range gets no scalar hint (its bare-literal form was handled
    // above; other forms self-describe their element).
    let hint = col.ty.array_element().and_then(|t| t.as_scalar());
    let (node, ty) = resolve(scope, e, hint, agg, params)?;
    if matches!(ty, ResolvedType::Null) {
        return Ok(node); // a NULL-typed expression (e.g. a CASE that may be NULL)
    }
    // Ranges/arrays compare equal only over equal element types (ResolvedType's derived Eq compares
    // the boxed element), matching the comparison rule (ranges.md §6 / array.md §5).
    if ty != col_rt {
        return Err(type_error(format!(
            "column {} is of type {} but expression is of type {}",
            col.name,
            col.ty.canonical_name(),
            ty.type_name()
        )));
    }
    Ok(node)
}

fn coerce_string_to_range_expr(
    text: &str,
    desc: &crate::ranges_gen::RangeDesc,
) -> Result<(RExpr, ResolvedType)> {
    let val = coerce_string_to_range(text, desc)?;
    let elem_rt = resolved_type_of(crate::range::element_scalar(desc));
    Ok((
        RExpr::ConstRange(Box::new(val)),
        ResolvedType::Range(Box::new(elem_rt)),
    ))
}

/// Parse a range text literal and coerce its bounds to the element type, producing a canonical
/// [`RangeVal`] (spec/design/ranges.md §4). The shared core of the range cast / typed-literal paths.
fn coerce_string_to_range(text: &str, desc: &crate::ranges_gen::RangeDesc) -> Result<RangeVal> {
    let parsed = crate::range::parse_range_text(text)?;
    if parsed.empty {
        return Ok(RangeVal::empty());
    }
    let elem = crate::range::element_scalar(desc);
    let coerce_bound = |b: &Option<String>| -> Result<Option<Value>> {
        match b {
            None => Ok(None),
            Some(s) => {
                let (node, _) = coerce_string_literal(s, elem, None, None)?;
                Ok(Some(rexpr_const_to_value(&node)?))
            }
        }
    };
    let lower = coerce_bound(&parsed.lower)?;
    let upper = coerce_bound(&parsed.upper)?;
    crate::range::finalize(desc, lower, upper, parsed.lower_inc, parsed.upper_inc)
}

fn coerce_string_literal(
    s: &str,
    target: ScalarType,
    typmod: Option<DecimalTypmod>,
    varchar_len: Option<u32>,
) -> Result<(RExpr, ResolvedType)> {
    Ok(match target {
        ScalarType::Bytea => (
            RExpr::ConstBytea(decode_bytea_literal(s)?),
            ResolvedType::Bytea,
        ),
        ScalarType::Uuid => (
            RExpr::ConstUuid(decode_uuid_literal(s)?),
            ResolvedType::Uuid,
        ),
        ScalarType::Timestamp => (
            RExpr::ConstTimestamp(parse_timestamp(s)?),
            ResolvedType::Timestamp,
        ),
        ScalarType::Timestamptz => (
            RExpr::ConstTimestamptz(parse_timestamptz(s)?),
            ResolvedType::Timestamptz,
        ),
        ScalarType::Interval => (
            RExpr::ConstInterval(parse_interval(s)?),
            ResolvedType::Interval,
        ),
        ScalarType::Date => (RExpr::ConstDate(parse_date(s)?), ResolvedType::Date),
        // `json '…'` / CAST('…' AS json) — validate well-formedness, store the bytes verbatim
        // (spec/design/json.md §4); malformed → 22P02.
        ScalarType::Json => {
            json::validate_json(s)?;
            (RExpr::ConstJson(s.to_string()), ResolvedType::Json)
        }
        // `jsonb '…'` / CAST('…' AS jsonb) — parse + canonicalize (numbers→decimal, keys deduped +
        // sorted — §2); malformed → 22P02.
        ScalarType::Jsonb => (
            RExpr::ConstJsonb(Box::new(json::jsonb_in(s)?)),
            ResolvedType::Jsonb,
        ),
        // `'…'::jsonpath` / `jsonpath '…'` — compile (P1a structural subset) + store the canonical
        // normalized text. Malformed → 42601; an unsupported (valid-PG) construct → 0A000.
        ScalarType::JsonPath => (
            RExpr::ConstJsonPath(crate::jsonpath::JsonPath::compile(s)?.render()),
            ResolvedType::JsonPath,
        ),
        // `text 'x'` is identity — the string IS the value. A `varchar(n) 'x'` typed literal /
        // `CAST('x' AS varchar(n))` silently truncates to n code points (the explicit-cast rule,
        // spec/design/types.md §15) — no 22001 at resolve.
        ScalarType::Text => (
            RExpr::ConstText(match varchar_len {
                Some(n) => truncate_to_chars(s, n as usize),
                None => s.to_string(),
            }),
            ResolvedType::Text,
        ),
        ScalarType::Bool => (RExpr::ConstBool(parse_bool_literal(s)?), ResolvedType::Bool),
        ScalarType::Decimal => {
            let d = parse_decimal_literal(s)?;
            let d = match typmod {
                Some(tm) => d.coerce_to_typmod(tm.precision as u32, tm.scale as u32)?,
                None => d.check_cap()?,
            };
            (RExpr::ConstDecimal(d), ResolvedType::Decimal)
        }
        ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64 => (
            RExpr::ConstInt(parse_int_literal(s, target)?),
            ResolvedType::Int(target),
        ),
        // `float '…'` / `real '…'` / CAST('…' AS f64) — parse via the float input function
        // (sign, digits, `.`, e-notation, Infinity/inf/NaN; spec/design/float.md §4). Malformed →
        // 22P02, out of range → 22003.
        ScalarType::Float64 => (
            RExpr::ConstFloat64(parse_f64_literal(s)?),
            ResolvedType::Float(ScalarType::Float64),
        ),
        ScalarType::Float32 => (
            RExpr::ConstFloat32(parse_f32_literal(s)?),
            ResolvedType::Float(ScalarType::Float32),
        ),
    })
}

/// Parse a string literal's content as a `f64` — the text→float coercion for `float '1.5e10'`
/// / `CAST('Infinity' AS f64)` (spec/design/float.md §4). Accepts an optional leading sign,
/// decimal digits with an optional point and `e`-notation, and the case-insensitive special words
/// `Infinity`/`+Infinity`/`-Infinity`/`inf`/`+inf`/`-inf`/`NaN` (PG `float8in` spellings).
/// Surrounding ASCII whitespace is trimmed. Malformed input traps `22P02`; a value outside the
/// binary64 range traps `22003`.
fn parse_f64_literal(s: &str) -> Result<f64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type f64: \"{s}\""),
        )
    };
    if let Some(v) = parse_float_special_f64(t) {
        return Ok(v);
    }
    // Rust's `f64::from_str` accepts the same finite grammar PG does (sign/digits/point/e-notation),
    // but also `inf`/`nan` spellings — already handled above, so reject any non-finite result that
    // sneaks through (defensive) and any parse failure.
    let v: f64 = t.parse().map_err(|_| invalid())?;
    if v.is_finite() {
        Ok(v)
    } else {
        // A finite-looking literal that overflows binary64 parses to ±Inf — that is 22003, not a
        // first-class infinity (only the special words above produce ±Inf).
        Err(overflow(ScalarType::Float64))
    }
}

/// As [`parse_f64_literal`], for `f32` (binary32). A finite value beyond the binary32 range
/// traps `22003`.
fn parse_f32_literal(s: &str) -> Result<f32> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type f32: \"{s}\""),
        )
    };
    if let Some(v) = parse_float_special_f32(t) {
        return Ok(v);
    }
    let v: f32 = t.parse().map_err(|_| invalid())?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(overflow(ScalarType::Float32))
    }
}

/// Recognize PG's special float spellings (case-insensitive): `infinity`/`inf` (± optional sign),
/// `nan`. Returns the value, or `None` if `t` is not one of them (a finite literal). Shared shape
/// for both widths.
fn parse_float_special_f64(t: &str) -> Option<f64> {
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "infinity" | "inf" => Some(sign * f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    }
}

/// As [`parse_float_special_f64`], at binary32.
fn parse_float_special_f32(t: &str) -> Option<f32> {
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "infinity" | "inf" => Some(sign * f32::INFINITY),
        "nan" => Some(f32::NAN),
        _ => None,
    }
}

/// Parse a string literal's content as a signed integer of type `ty` — the text→integer coercion
/// for `INTEGER '42'` / `CAST('42' AS int)` (grammar.md §36). Matches jed's OWN integer-literal
/// grammar: surrounding ASCII whitespace trimmed, an optional leading `+`/`-`, then one or more
/// ASCII decimal digits. NO hex/octal/binary or digit underscores (those trap `22P02`, a documented
/// PG divergence). A value outside `ty`'s range traps `22003`; anything else `22P02`.
fn parse_int_literal(s: &str, ty: ScalarType) -> Result<i64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!(
                "invalid input syntax for type {}: \"{s}\"",
                ty.canonical_name()
            ),
        )
    };
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    // All-digit but too large for i128 is an out-of-range value (22003), not malformed (22P02).
    let mag: i128 = digits.parse().map_err(|_| overflow(ty))?;
    let val = if neg { -mag } else { mag };
    if val < ty.min() as i128 || val > ty.max() as i128 {
        return Err(overflow(ty));
    }
    Ok(val as i64)
}

/// Parse a string literal's content as a decimal — the text→decimal coercion for `NUMERIC '1.5'`
/// / `CAST('1.5' AS numeric)` (grammar.md §36). Matches jed's OWN decimal-literal grammar: trimmed
/// ASCII whitespace, optional sign, ASCII digits with at most one `.` and a digit on at least one
/// side, plus optional scientific `e`-notation (`numeric '1.5e3'` → `1500`) — built into the SAME
/// `(digits, scale)` the lexer feeds `from_digits_scale` (via the shared `decimal_from_parts`), so a
/// `NUMERIC 'x'` is byte-identical to writing `x`. NO `NaN` / `Infinity` and no hex/underscore
/// (those trap `22P02` — jed's decimal is always finite; documented PG divergences). The caller
/// applies the typmod / cap-check.
fn parse_decimal_literal(s: &str) -> Result<Decimal> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type numeric: \"{s}\""),
        )
    };
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Split off an optional exponent. Unlike the lexer (which leaves a bare `e` for the next
    // token), an isolated string must be a COMPLETE numeric, so an `e` with no `[+-]?digit+`
    // after it is malformed (`22P02`), matching PG's `numeric_in`.
    let (mantissa, exp) = match rest.find(|c: char| c == 'e' || c == 'E') {
        Some(pos) => {
            let (m, e) = (&rest[..pos], &rest[pos + 1..]);
            let (eneg, edigits) = match e.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, e.strip_prefix('+').unwrap_or(e)),
            };
            if edigits.is_empty() || !edigits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            // Clamp the magnitude to `EXP_LIMIT` while accumulating (keeps it in `i64` and
            // bounds the coefficient the shared builder may materialize).
            let mut v: i64 = 0;
            for b in edigits.bytes() {
                if v < decimal::EXP_LIMIT {
                    v = v * 10 + (b - b'0') as i64;
                    if v > decimal::EXP_LIMIT {
                        v = decimal::EXP_LIMIT;
                    }
                }
            }
            (m, Some(if eneg { -v } else { v }))
        }
        None => (rest, None),
    };
    let mut parts = mantissa.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    // A second `.` lands in `frac` (splitn(2) does not split it); reject it.
    if frac.contains('.')
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac.is_empty())
    {
        return Err(invalid());
    }
    let (digits, scale) = decimal::decimal_from_parts(int_part, frac, exp);
    Ok(Decimal::from_digits_scale(neg, &digits, scale))
}

/// Parse a string literal's content as a boolean — the text→boolean coercion for `BOOLEAN 'true'`
/// / `CAST('t' AS boolean)` (grammar.md §36). Matches PostgreSQL's `boolin`: trimmed ASCII
/// whitespace, case-insensitive; `t`/`tr`/`tru`/`true`, `y`/`ye`/`yes`, `on`, `1` → true and
/// `f`/`fa`/`fal`/`fals`/`false`, `n`/`no`, `off`, `0` → false; anything else `22P02`.
fn parse_bool_literal(s: &str) -> Result<bool> {
    let t = s
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    match t.as_str() {
        "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" | "1" => Ok(true),
        "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "off" | "0" => Ok(false),
        _ => Err(EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type boolean: \"{s}\""),
        )),
    }
}

/// A resolved `ON CONFLICT` clause (spec/design/upsert.md), built by `resolve_on_conflict`.
pub(crate) struct ConflictPlan {
    /// The arbiter constraint whose violation triggers the action. `None` only with
    /// `DoNothing` (any uniqueness conflict is then skipped).
    arbiter: Option<Arbiter>,
    action: ConflictActionPlan,
}

/// Which uniqueness constraint an `ON CONFLICT` arbitrates (spec/design/upsert.md §2).
pub(crate) enum Arbiter {
    /// The primary key — the arbiter key is the storage key.
    PrimaryKey,
    /// A unique index, by position in the table's `indexes` list.
    Index(usize),
}

/// The resolved `ON CONFLICT` action (spec/design/upsert.md §5).
pub(crate) enum ConflictActionPlan {
    DoNothing,
    DoUpdate {
        assignments: Vec<AssignPlan>,
        filter: Option<RExpr>,
    },
}

/// Resolve an `ON CONFLICT` target into an `Arbiter` (spec/design/upsert.md §2): a column list is
/// matched as an order-independent SET against a unique index / the primary key (no match →
/// 42P10); `ON CONSTRAINT name` names a unique index or the synthesized `<table>_pkey` (miss →
/// 42704). `None` target → `None` arbiter (legal only with `DO NOTHING`).
fn resolve_arbiter(tdef: &Table, target: Option<&ConflictTarget>) -> Result<Option<Arbiter>> {
    let target = match target {
        None => return Ok(None),
        Some(t) => t,
    };
    let pk = tdef.pk_indices();
    match target {
        ConflictTarget::Columns(cols) => {
            let mut want = std::collections::BTreeSet::new();
            for c in cols {
                want.insert(col_idx(tdef, c)?); // unknown column → 42703
            }
            if !pk.is_empty()
                && pk
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == want
            {
                return Ok(Some(Arbiter::PrimaryKey));
            }
            for (i, def) in tdef.indexes.iter().enumerate() {
                if def.unique
                    && def
                        .columns
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        == want
                {
                    return Ok(Some(Arbiter::Index(i)));
                }
            }
            Err(EngineError::new(
                SqlState::InvalidColumnReference,
                "there is no unique or exclusion constraint matching the ON CONFLICT specification",
            ))
        }
        ConflictTarget::Constraint(name) => {
            let pkey = format!("{}_pkey", tdef.name.to_ascii_lowercase());
            if !pk.is_empty() && name.eq_ignore_ascii_case(&pkey) {
                return Ok(Some(Arbiter::PrimaryKey));
            }
            if let Some(i) = tdef
                .indexes
                .iter()
                .position(|d| d.unique && d.name.eq_ignore_ascii_case(name))
            {
                return Ok(Some(Arbiter::Index(i)));
            }
            Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("constraint {} for table {} does not exist", name, tdef.name),
            ))
        }
    }
}

/// The arbiter key of a candidate row (spec/design/upsert.md §3): the storage key for a PK
/// arbiter (never NULL), or the unique-index prefix for an index arbiter (`None` when a nullable
/// arbiter column is NULL — NULLS DISTINCT, so the row never conflicts).
fn arbiter_key(
    arb: &Arbiter,
    pk: &[(usize, Type)],
    colls: &[Option<std::sync::Arc<Collation>>],
    columns: &[Column],
    indexes: &[IndexDef],
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    match arb {
        Arbiter::PrimaryKey => Ok(Some(encode_pk_key(pk, colls, row)?)),
        Arbiter::Index(i) => index_prefix_key(columns, colls, &indexes[*i], row),
    }
}

/// A resolved UPDATE assignment: which column to write, the target type/nullability so
/// the new value is re-checked exactly like INSERT, and the resolved RHS expression
/// (evaluated against the *old* row).
pub(crate) struct AssignPlan {
    idx: usize,
    name: String,
    target: ScalarType,
    decimal: Option<DecimalTypmod>,
    /// The `varchar(n)` length for a text column (spec/design/types.md §15) — UPDATE re-checks
    /// the new value's length exactly like INSERT (over-length 22001, trailing-space truncate).
    varchar_len: Option<u32>,
    not_null: bool,
    source: RExpr,
    /// The resolved `ColType` for a NON-scalar (range / array) column — `Some` ⇒ `check` stores
    /// through `coerce_for_store` (the container codec, ranges.md §4 / array.md §4); `None` for a
    /// scalar column, which stays on the `store_value` fast path. Composite columns are deferred
    /// (0A000) at resolution, so they never reach here.
    col_type: Option<ColType>,
}
