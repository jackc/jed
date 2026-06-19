//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{
    AlterSequence, BinaryOp, CreateIndex, CreateSequence, CreateTable, CreateType, Delete,
    DropIndex, DropSequence, DropTable, DropType, Expr, Insert, InsertSource, InsertValue,
    JoinKind, Literal, OrderKey, QueryExpr, RefAction, Select, SelectItems, SetOp, SetOpKind,
    Statement, SubscriptSpec, TableRef, TypeMod, UnaryOp, Update, WithQuery,
};
use crate::catalog::{
    CheckConstraint, ColField, ColType, Column, CompositeField, CompositeType, DefaultExpr,
    FkAction, ForeignKeyConstraint, IndexDef, IndexKind, SequenceDef, Table, resolve_col_type,
};
use crate::cost::Meter;
use crate::costs::COSTS;
use crate::date::parse_date;
use crate::decimal::{self, Decimal, MAX_PRECISION, MAX_SCALE};
use crate::encoding::{encode_bool, encode_int};
use crate::error::{EngineError, Result, SqlState};
use crate::interval::{self, Interval, parse_interval};
use crate::operators::{AGGREGATES, AggregateDesc, OPERATORS, OperatorDesc};
use crate::pmap::KeyBound;
use crate::storage::{Row, TableStore};
use crate::timestamp::{parse_timestamp, parse_timestamptz};
use crate::types::{DecimalTypmod, ScalarType, Type};
use crate::value::{
    ArrayVal, ThreeValued, Value, and3, from3, not3, or3, parse_bytea_hex, parse_uuid,
};
use std::collections::{BTreeSet, HashMap, HashSet};

/// The outcome of executing one statement. Both variants carry the deterministic
/// execution `cost` accrued while running the statement (CLAUDE.md §13) — a DML
/// statement accrues its scan + filter cost even though it returns no rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
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

/// The default per-handle input-SQL byte limit (1 MiB — CLAUDE.md §13; spec/design/api.md §8,
/// cost.md §7). The §13 input-size gate's default ceiling: generous for hand-written / ORM SQL,
/// yet bounds the parse tree to a few MB so unbounded untrusted input cannot exhaust memory. A
/// caller raises it (trusted bulk loads) or sets `0` for unlimited via
/// [`Database::set_max_sql_length`]. Identical across cores (§8).
pub const DEFAULT_MAX_SQL_LENGTH: usize = 1 << 20;

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
#[derive(Clone, Default)]
pub struct Snapshot {
    /// The snapshot's version — the commit counter (transactions.md §8; the watermark unit).
    pub(crate) txid: u64,
    tables: HashMap<String, Table>,
    /// User-defined composite (row) types, keyed by lowercased name (spec/design/composite.md).
    /// A database-level object set, separate from `tables`; serialized into the catalog's
    /// composite-type entries (spec/fileformat/format.md). Sorted by key when serialized so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    types: HashMap<String, CompositeType>,
    stores: HashMap<String, TableStore>,
    /// Each secondary index's B-tree (spec/design/indexes.md §3): a `TableStore` with ZERO
    /// value columns (entry keys only — the on-disk empty-payload record), keyed by the
    /// lowercased index name (index names live in the relation namespace, globally unique).
    /// Which table owns an index is recorded in that table's `Table::indexes`.
    index_stores: HashMap<String, TableStore>,
    /// Sequences, keyed by lowercased name (spec/design/sequences.md). A database-level object set
    /// separate from `tables`/`types`; serialized into the catalog's sequence entries
    /// (spec/fileformat/format.md, `entry_kind = 2`). The mutable counter (`last_value`/`is_called`)
    /// lives here, so `nextval` advances the working snapshot and rolls back with it (sequences.md §5).
    sequences: HashMap<String, SequenceDef>,
}

impl Snapshot {
    /// Look up a table definition by name (case-insensitive).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// Look up a composite type definition by name (case-insensitive).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.types.get(&name.to_ascii_lowercase())
    }

    /// Register a composite type (CREATE TYPE). Lower-cased name is the key. The caller has
    /// already resolved field types and checked for a duplicate.
    pub(crate) fn put_type(&mut self, ty: CompositeType) {
        self.types.insert(ty.name.to_ascii_lowercase(), ty);
    }

    /// Remove a composite type (DROP TYPE). The caller has checked there are no dependents.
    pub(crate) fn remove_type(&mut self, key: &str) {
        self.types.remove(key);
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
        self.sequences.insert(seq.name.to_ascii_lowercase(), seq);
    }

    /// Remove a sequence (DROP SEQUENCE). The caller has checked it exists.
    pub(crate) fn remove_sequence(&mut self, key: &str) {
        self.sequences.remove(key);
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

    /// Whether any OTHER table's FOREIGN KEY references the table `name` (case-insensitive) — the
    /// `DROP TABLE` dependency check (2BP01 — spec/design/constraints.md §6.10). A self-reference
    /// does NOT block the drop (a table's own FK on itself disappears with it). Returns the first
    /// dependent's description, scanning in ascending lowercased table-name order for determinism
    /// (within a table, `foreign_keys` is already in name order).
    pub(crate) fn foreign_key_dependent(&self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        let mut tkeys: Vec<&String> = self.tables.keys().collect();
        tkeys.sort();
        for tk in tkeys {
            let t = &self.tables[tk];
            if t.name.eq_ignore_ascii_case(&key) {
                continue; // a self-reference does not block the drop
            }
            for fk in &t.foreign_keys {
                if fk.ref_table.eq_ignore_ascii_case(&key) {
                    return Some(format!("constraint {} on table {}", fk.name, t.name));
                }
            }
        }
        None
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
        self.stores
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table is absent. A
    /// test/debug convenience (the SELECT path scans through `iter_in_key_order` directly, propagating
    /// I/O errors); every value is fully materialized — the helper's callers compare whole rows, so
    /// no unfetched reference may escape (large-values.md §14). The fault-`Result` is unwrapped here.
    fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.stores.get(&name.to_ascii_lowercase()).map(|s| {
            let mut rows = s.iter_in_key_order().expect("test-helper read failed");
            for row in &mut rows {
                s.resolve_all(row).expect("test-helper resolve failed");
            }
            rows
        })
    }

    /// Register a new table and its (empty) store. Lower-cased name is the key. The store carries
    /// the page payload `cap` (= `page_size − 12`) and the column types so the page-backed B-tree
    /// can weigh records for its size-driven split (spec/fileformat/format.md).
    pub(crate) fn put_table(&mut self, table: Table, page_size: u32) {
        let key = table.name.to_ascii_lowercase();
        let cap = page_size as usize - 12; // PAGE_HEADER
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
        self.stores
            .insert(key.clone(), TableStore::new(cap, col_types));
        self.tables.insert(key, table);
    }

    /// Remove a table's definition, its store, and its indexes' stores (DROP TABLE — the
    /// indexes have no independent life, spec/design/indexes.md §2).
    fn remove_table(&mut self, key: &str) {
        if let Some(t) = self.tables.get(key) {
            for idx in &t.indexes {
                self.index_stores.remove(&idx.name.to_ascii_lowercase());
            }
        }
        self.tables.remove(key);
        self.stores.remove(key);
    }

    /// The store of a secondary index (panics if absent — callers resolve the index first).
    pub(crate) fn index_store(&self, name_key: &str) -> &TableStore {
        self.index_stores
            .get(name_key)
            .expect("store exists for a resolved index")
    }

    /// The store of a secondary index, mutable (panics if absent).
    pub(crate) fn index_store_mut(&mut self, name_key: &str) -> &mut TableStore {
        self.index_stores
            .get_mut(name_key)
            .expect("store exists for a resolved index")
    }

    /// Register a new (empty) secondary index on `table_key`: insert its definition into the
    /// table's `indexes` in ascending lowercased-name order (the catalog/planner order —
    /// spec/design/indexes.md §6) and create its zero-column store.
    pub(crate) fn put_index(&mut self, table_key: &str, def: IndexDef, page_size: u32) {
        let name_key = def.name.to_ascii_lowercase();
        let cap = page_size as usize - 12; // PAGE_HEADER
        self.index_stores
            .insert(name_key.clone(), TableStore::new(cap, Vec::new()));
        let table = self.tables.get_mut(table_key).expect("table exists");
        let pos = table
            .indexes
            .iter()
            .position(|i| i.name.to_ascii_lowercase() > name_key)
            .unwrap_or(table.indexes.len());
        table.indexes.insert(pos, def);
    }

    /// Register a loaded index store under its (lowercased) name — the file loader's hook
    /// (format.rs): the owning table's `indexes` list came from its catalog entry, so only
    /// the store is registered here.
    pub(crate) fn put_index_store(&mut self, name_key: String, store: TableStore) {
        self.index_stores.insert(name_key, store);
    }

    /// Remove one secondary index (DROP INDEX): its definition from the owning table and
    /// its store.
    fn remove_index(&mut self, table_key: &str, name_key: &str) {
        if let Some(t) = self.tables.get_mut(table_key) {
            t.indexes
                .retain(|i| i.name.to_ascii_lowercase() != name_key);
        }
        self.index_stores.remove(name_key);
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
    /// The shared paging context for a file-backed database (spec/design/pager.md): the open pager
    /// (kept for the handle's life) + the bounded leaf buffer pool, shared (`Arc`) with every table
    /// store so reads fault `OnDisk` leaves through the one pool. The load reads pages through it and
    /// every commit writes through it. `None` for an in-memory database (`persist` is then a no-op);
    /// set by `open`/`create`, dropped by `close`.
    pub(crate) paging: Option<std::sync::Arc<crate::paging::SharedPaging>>,
    /// The caller-set execution-cost ceiling (CLAUDE.md §13; spec/design/api.md §8), or `0`
    /// (the default) for **unlimited**. A positive value bounds every statement run on this
    /// handle: each statement's [`Meter`](crate::cost::Meter) is built with this limit and
    /// aborts with `54P01` the instant accrued cost reaches it. A handle setting (not stored
    /// in the file), set by [`set_max_cost`](Database::set_max_cost); the primary guard for
    /// safely evaluating untrusted, user-supplied queries.
    pub(crate) max_cost: i64,
    /// The maximum input SQL length, in **bytes**, accepted on this handle (CLAUDE.md §13;
    /// spec/design/api.md §8, cost.md §7). Default [`DEFAULT_MAX_SQL_LENGTH`] (1 MiB); `0` ⇒
    /// **unlimited** (a trusted caller's opt-out). A statement whose text exceeds it is rejected
    /// with `54000` at [`parse`](Database::parse) — **before** lexing — so unbounded input cannot
    /// exhaust parse memory/CPU (the §13 input-size gate, which the cost meter cannot catch
    /// because parsing precedes metering). A handle setting (not stored in the file), set by
    /// [`set_max_sql_length`](Database::set_max_sql_length).
    pub(crate) max_sql_length: usize,
    /// Whether this handle was opened **read-only** (spec/design/api.md §2.1,
    /// [`crate::file::OpenOptions::read_only`]). A read-only handle behaves like PostgreSQL
    /// hot standby: every transaction defaults to READ ONLY, an explicit `BEGIN READ WRITE`
    /// (or `begin(true)`) is `25006`, and an autocommit write is `25006` — so no commit ever
    /// publishes and the file is never written (it is opened without write access). Always
    /// `false` for an in-memory or normally-opened database.
    pub(crate) read_only: bool,
    /// The work-memory budget in **bytes** (spec/design/spill.md §2, api.md §2.1): the memory a
    /// single blocking operator (currently the `ORDER BY` external merge sort) may hold resident
    /// before it spills sorted runs to disk. A handle setting (not stored in the file), set by
    /// [`set_work_mem`](Database::set_work_mem); `0` ⇒ **unlimited** (never spill). It never changes
    /// what a query observes (results + cost are invariant, spill.md §6) — only when an operator
    /// spills. An **in-memory** database ignores it (no backing file to spill to — it stays fully
    /// resident, like the buffer pool). Default [`DEFAULT_WORK_MEM`](crate::spill::DEFAULT_WORK_MEM).
    pub(crate) work_mem: usize,
    /// The entropy + clock seam for the uuid generators (spec/design/entropy.md): two host-injectable
    /// functions (a random source + a clock), each defaulting to the platform primitive (OS CSPRNG
    /// per value / wall clock). Set by [`set_random_source`](Database::set_random_source) /
    /// [`set_clock_source`](Database::set_clock_source). Tests inject the provided
    /// [`seeded_random_source`](crate::seam::seeded_random_source) + [`fixed_clock`](crate::seam::fixed_clock)
    /// (the `# seed:` / `# clock:` directives) for exact cross-core output. A handle setting, not
    /// stored in the file; does not affect a query that calls no generator.
    pub(crate) seam: crate::seam::Seam,
    /// Per-handle **session** `currval` state (spec/design/sequences.md §6): the last value
    /// `nextval`/`setval(…,true)` produced **in this session** for each sequence (lowercased name).
    /// NOT part of the snapshot and NOT persisted — strictly session-local, as in PostgreSQL.
    /// Updated when a sequence-advancing statement succeeds (flushing `pending_currval`); `currval`
    /// of an unlisted sequence this session is `55000`.
    session_seq: HashMap<String, i64>,
    /// Per-handle **session** `lastval` state (spec/design/sequences.md §6): the lowercased **name**
    /// of the sequence the most recent `nextval` (of **any** sequence) ran on — `None` before the
    /// first `nextval`. `lastval()` returns the *current* session value of that sequence (PG: it
    /// reads the last-used sequence's cached value), so a `setval` on that same sequence is
    /// reflected; a `setval` never changes *which* sequence this points to. `55000` when `None`.
    session_last_name: Option<String>,
    /// Per-**statement** running sequence advances (spec/design/sequences.md §4), behind a `RefCell`
    /// for interior mutability — `EvalEnv` borrows `&Database`, so a `nextval`/`setval` records its
    /// advance here (seeded from the working snapshot on first touch), and later calls in the same
    /// statement see the running state. On statement success it is flushed into the working snapshot
    /// (so commit persists it); on error it is discarded (the transactional rollback of the advance,
    /// sequences.md §5). Cleared at the start of every statement.
    pending_seq: std::cell::RefCell<HashMap<String, SequenceDef>>,
    /// Per-**statement** running `currval` updates (the names `nextval`/`setval(…,true)` touched
    /// this statement → their produced value). Kept separate from `pending_seq` because `currval` is
    /// updated by a *subset* of catalog mutations: `setval(…,false)` and `ALTER … RESTART` advance
    /// the counter without defining `currval`. Flushed into `session_seq` on statement success.
    pending_currval: std::cell::RefCell<HashMap<String, i64>>,
    /// Per-**statement** running `lastval` update (the lowercased name of the most recent `nextval`
    /// this statement, `None` if no `nextval` ran). `setval` never sets it. Flushed into
    /// `session_last_name` on success.
    pending_last_name: std::cell::RefCell<Option<String>>,
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
    /// The handle's `currval`/`lastval` session state (spec/design/sequences.md §6) captured when
    /// this transaction opened. A `nextval`/`setval` inside the block updates the handle's session
    /// state per-statement (so an in-block `currval` sees its own advance), but those updates must
    /// **roll back** with the transaction (§5) — so ROLLBACK (and a failed/read-only COMMIT)
    /// restores these, while a successful COMMIT keeps the advanced state.
    saved_session_seq: HashMap<String, i64>,
    saved_session_last_name: Option<String>,
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
            paging: None,
            max_cost: 0,
            max_sql_length: DEFAULT_MAX_SQL_LENGTH,
            read_only: false,
            work_mem: crate::spill::DEFAULT_WORK_MEM,
            seam: crate::seam::Seam::default(),
            session_seq: HashMap::new(),
            session_last_name: None,
            pending_seq: std::cell::RefCell::new(HashMap::new()),
            pending_currval: std::cell::RefCell::new(HashMap::new()),
            pending_last_name: std::cell::RefCell::new(None),
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
            paging: None,
            max_cost: 0,
            max_sql_length: DEFAULT_MAX_SQL_LENGTH,
            read_only: false,
            work_mem: crate::spill::DEFAULT_WORK_MEM,
            seam: crate::seam::Seam::default(),
            session_seq: HashMap::new(),
            session_last_name: None,
            pending_seq: std::cell::RefCell::new(HashMap::new()),
            pending_currval: std::cell::RefCell::new(HashMap::new()),
            pending_last_name: std::cell::RefCell::new(None),
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

    /// `nextval('name')` (spec/design/sequences.md §4): advance the named sequence and return the
    /// new value. Interior-mutable (the evaluator borrows `&Database`): the running state lives in
    /// `pending_seq`, seeded from the working snapshot on first touch this statement, and is flushed
    /// into the working snapshot + `session_seq` on statement success ([`flush_pending_sequences`]).
    /// A missing sequence is 42P01; advancing past a bound without CYCLE is 2200H.
    fn seq_nextval(&self, name: &str) -> Result<i64> {
        let key = name.to_ascii_lowercase();
        let mut pending = self.pending_seq.borrow_mut();
        let mut def = match pending.get(&key) {
            Some(d) => d.clone(),
            None => self.read_snap().sequence(name).cloned().ok_or_else(|| {
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
        self.pending_currval
            .borrow_mut()
            .insert(key.clone(), result);
        *self.pending_last_name.borrow_mut() = Some(key);
        Ok(result)
    }

    /// `setval('name', n)` / `setval('name', n, is_called)` (spec/design/sequences.md §4): set the
    /// sequence's counter directly and return `n`. A missing sequence is 42P01; `n` outside
    /// `[min_value, max_value]` is 22003. `last_value = n`, `is_called` = the flag (default true);
    /// when `is_called` is true the value also defines this session's `currval` (PG: `is_called =
    /// false` leaves `currval` untouched). `setval` never updates `lastval` (PG — §6).
    fn seq_setval(&self, name: &str, n: i64, is_called: bool) -> Result<i64> {
        let key = name.to_ascii_lowercase();
        let mut pending = self.pending_seq.borrow_mut();
        let mut def = match pending.get(&key) {
            Some(d) => d.clone(),
            None => self.read_snap().sequence(name).cloned().ok_or_else(|| {
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
            self.pending_currval.borrow_mut().insert(key, n);
        }
        Ok(n)
    }

    /// `lastval()` (spec/design/sequences.md §6): the **current** session value of the sequence the
    /// most recent `nextval` (of any sequence) ran on IN THIS SESSION — PG reads the last-used
    /// sequence's cached value, so a `setval` on that same sequence is reflected, while a `setval`
    /// on a *different* sequence is not (it does not change which sequence this points to). Takes no
    /// name argument (no 42P01 path); `55000` before the first `nextval` this session. The effective
    /// name and its value both honor the statement's running updates over the session state.
    fn seq_lastval(&self) -> Result<i64> {
        let name = self
            .pending_last_name
            .borrow()
            .clone()
            .or_else(|| self.session_last_name.clone());
        let key = match name {
            Some(k) => k,
            None => {
                return Err(EngineError::new(
                    SqlState::ObjectNotInPrerequisiteState,
                    "lastval is not yet defined in this session".to_string(),
                ));
            }
        };
        if let Some(v) = self.pending_currval.borrow().get(&key) {
            return Ok(*v);
        }
        if let Some(v) = self.session_seq.get(&key) {
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
    fn seq_currval(&self, name: &str) -> Result<i64> {
        if self.read_snap().sequence(name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("relation does not exist: {name}"),
            ));
        }
        let key = name.to_ascii_lowercase();
        if let Some(v) = self.pending_currval.borrow().get(&key) {
            return Ok(*v);
        }
        if let Some(v) = self.session_seq.get(&key) {
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
    fn flush_pending_sequences(&mut self) {
        let pending = std::mem::take(&mut *self.pending_seq.borrow_mut());
        for def in pending.into_values() {
            self.working_mut().put_sequence(def);
        }
        let currvals = std::mem::take(&mut *self.pending_currval.borrow_mut());
        for (key, v) in currvals {
            self.session_seq.insert(key, v);
        }
        if let Some(name) = self.pending_last_name.borrow_mut().take() {
            self.session_last_name = Some(name);
        }
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
        self.max_cost = limit;
    }

    /// Set the maximum input SQL length, in **bytes**, accepted on this handle (CLAUDE.md §13;
    /// spec/design/api.md §8). A statement whose text exceeds `bytes` is rejected with `54000`
    /// at parse entry, before lexing — the §13 input-size gate (cost.md §7). `0` is **unlimited**
    /// (a trusted caller's opt-out); the default is [`DEFAULT_MAX_SQL_LENGTH`] (1 MiB). A handle
    /// setting, not stored in the file (mirrors `set_max_cost`).
    pub fn set_max_sql_length(&mut self, bytes: usize) {
        self.max_sql_length = bytes;
    }

    /// The current input-SQL byte limit (`0` ⇒ unlimited). See [`set_max_sql_length`](Database::set_max_sql_length).
    pub fn max_sql_length(&self) -> usize {
        self.max_sql_length
    }

    /// Whether this handle was opened read-only (spec/design/api.md §2.1): every transaction
    /// defaults to READ ONLY, writes are `25006`, and the file is never written.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The current execution-cost ceiling (`0` ⇒ unlimited). See [`set_max_cost`](Database::set_max_cost).
    pub fn max_cost(&self) -> i64 {
        self.max_cost
    }

    /// Inject a random source for the uuid generators (spec/design/entropy.md §6) — the
    /// deterministic / reproducible path. Pass [`seeded_random_source`](crate::seam::seeded_random_source)
    /// for a byte-identical cross-core stream (the conformance path; tests use the `# seed:`
    /// directive). A handle setting, not stored in the file.
    pub fn set_random_source(&mut self, f: crate::seam::RandomSource) {
        self.seam.set_random(f);
    }

    /// Clear the injected random source: the generators return to the OS CSPRNG, drawn per value
    /// (production — unpredictable output).
    pub fn clear_random_source(&mut self) {
        self.seam.clear_random();
    }

    /// Inject a clock source for `uuidv7` (entropy.md §6) — e.g. [`fixed_clock`](crate::seam::fixed_clock)
    /// (the `# clock:` directive). After this, `uuidv7()` embeds the source's instant instead of the
    /// wall clock. A handle setting, not stored in the file.
    pub fn set_clock_source(&mut self, f: crate::seam::ClockSource) {
        self.seam.set_clock(f);
    }

    /// Clear the injected clock source: `uuidv7` returns to reading the wall clock (production).
    pub fn clear_clock_source(&mut self) {
        self.seam.clear_clock();
    }

    /// Set the work-memory budget (in **bytes**) for blocking operators run on this handle
    /// (spec/design/spill.md §3, api.md §2.1): the `ORDER BY` external merge sort holds at most
    /// roughly this many bytes of rows resident before it spills sorted runs to disk. `0` is
    /// **unlimited** (never spill). It never changes what a query observes (results + cost are
    /// invariant — spill.md §6), only when an operator spills; an in-memory database ignores it (no
    /// file to spill to). A handle setting, not stored in the file (mirrors `set_max_cost`).
    pub fn set_work_mem(&mut self, bytes: usize) {
        self.work_mem = bytes;
    }

    /// The current work-memory budget in bytes (`0` ⇒ unlimited). See [`set_work_mem`](Database::set_work_mem).
    pub fn work_mem(&self) -> usize {
        self.work_mem
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

    /// Look up a composite type definition by name (case-insensitive) in the currently-visible
    /// snapshot (spec/design/composite.md).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.read_snap().composite_type(name)
    }

    /// The canonical name of every table in the currently-visible snapshot, sorted ascending
    /// by lowercased name (the catalog's standing order — no map-iteration order may leak,
    /// CLAUDE.md §8). Secondary indexes are not tables and are excluded (api.md §6).
    pub fn table_names(&self) -> Vec<String> {
        let snap = self.read_snap();
        let mut named: Vec<(&str, &str)> = snap
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
        self.pending_seq.borrow_mut().clear();
        self.pending_currval.borrow_mut().clear();
        *self.pending_last_name.borrow_mut() = None;

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
            if result.is_ok() {
                // Land any nextval advances into the block's working snapshot; COMMIT publishes
                // them, ROLLBACK discards them with the rest of the working set (sequences.md §5).
                self.flush_pending_sequences();
            } else {
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
        self.tx = Some(ActiveTx {
            writable: true,
            failed: false,
            working: self.committed.clone(),
            saved_session_seq: self.session_seq.clone(),
            saved_session_last_name: self.session_last_name.clone(),
        });
        match self.dispatch_stmt(stmt, params) {
            Ok(outcome) => {
                // Persist any nextval advances into the working snapshot before publishing it
                // (sequences.md §5); a non-sequence statement flushes nothing.
                self.flush_pending_sequences();
                self.commit_tx().map(|_| outcome)
            }
            Err(e) => {
                // The statement failed before any flush, so session state is untouched; restore
                // from the captured copy anyway to keep the discard path uniform (sequences.md §6).
                if let Some(tx) = self.tx.take() {
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
        if self.tx.is_some() {
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
        self.tx = Some(ActiveTx {
            writable: writable.unwrap_or(!self.read_only),
            failed: false,
            working: self.committed.clone(),
            saved_session_seq: self.session_seq.clone(),
            saved_session_last_name: self.session_last_name.clone(),
        });
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Restore the handle's `currval`/`lastval` session state from a discarded transaction's
    /// captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block `nextval`/
    /// `setval` session updates. Called wherever a transaction is dropped without publishing.
    fn restore_session_state(&mut self, tx: ActiveTx) {
        self.session_seq = tx.saved_session_seq;
        self.session_last_name = tx.saved_session_last_name;
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
            None => {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            Some(tx) => tx,
        };
        if tx.failed || !tx.writable {
            // A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG),
            // so any in-block session updates revert with the discarded working set (§5/§6).
            self.restore_session_state(tx);
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
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
        if let Some(tx) = self.tx.take() {
            self.restore_session_state(tx);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
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
            Statement::CreateIndex(ci) => {
                reject_params_for_ddl(params)?;
                self.execute_create_index(ci)
            }
            Statement::DropIndex(di) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_index(di)
            }
            Statement::CreateType(ct) => {
                reject_params_for_ddl(params)?;
                self.execute_create_type(ct)
            }
            Statement::DropType(dt) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_type(dt)
            }
            Statement::CreateSequence(cs) => {
                reject_params_for_ddl(params)?;
                self.execute_create_sequence(cs)
            }
            Statement::DropSequence(ds) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_sequence(ds)
            }
            Statement::AlterSequence(als) => {
                reject_params_for_ddl(params)?;
                self.execute_alter_sequence(als)
            }
            Statement::Insert(ins) => self.execute_insert(ins, params),
            Statement::Select(sel) => self.execute_select(sel, params),
            Statement::SetOp(so) => self.execute_set_op(so, params),
            Statement::With(wq) => self.execute_with(wq, params),
            Statement::Update(upd) => self.execute_update(upd, params),
            Statement::Delete(del) => self.execute_delete(del, params),
            // Transaction control is handled by `execute_stmt_params` before dispatch.
            Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {
                unreachable!("transaction control is handled before dispatch")
            }
        }
    }

    /// Analyze and run a CREATE TABLE: resolve each column's type name, enforce a
    /// single primary key across both forms (column-level and the table-level
    /// `PRIMARY KEY (a, b, …)` constraint — which is implicitly NOT NULL per member),
    /// reject duplicate table and column names, then register the table.
    /// Constraint checks mirror PostgreSQL's order (oracle-probed, constraints.md §3):
    /// a second primary key traps 42P16 before its members resolve; members resolve
    /// left to right (unknown 42703, repeated 42701); then the jed narrowings — the
    /// declaration-order rule and the per-member key-type gate — trap 0A000.
    fn execute_create_table(&mut self, ct: CreateTable) -> Result<Outcome> {
        // The relation namespace is shared between tables and indexes (indexes.md §2), so a
        // CREATE TABLE colliding with either kind is the same 42P07 — PG's "relation" word.
        if self.relation_exists(&ct.name) {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", ct.name),
            ));
        }

        let mut columns = Vec::with_capacity(ct.columns.len());
        // The primary-key member ordinals in KEY order (constraints.md §3): the column-level
        // form is the one-member case; the table-level list below records its own order.
        let mut pk: Vec<usize> = Vec::new();
        let mut pk_seen = false;
        // The OWNED sequences a `serial` column desugars to (spec/design/sequences.md §12), collected
        // during the column walk and staged into the working snapshot only after the whole CREATE
        // TABLE validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
        let mut pending_serials: Vec<SequenceDef> = Vec::new();
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
            // A `serial` / `bigserial` / `smallserial` pseudo-type (spec/design/sequences.md §12):
            // CREATE TABLE sugar for an integer column that is NOT NULL with a DEFAULT nextval(...)
            // backed by a newly-created OWNED sequence. The desugaring (the owned sequence + the
            // default + the NOT NULL force) happens in the default-classification block and the
            // column push below; here we only resolve the underlying integer type. `serial[]` is NOT
            // a serial column (it falls to the array branch as an unknown element type — §12.1).
            let serial_kind = serial_pseudo_type(&def.type_name);
            // Resolve the column type: a built-in scalar, or a user-defined composite referenced by
            // name (spec/design/composite.md §3). An unknown name is 42704. A composite column
            // carries no typmod (the composite's fields carry their own); a type modifier written on
            // a composite column is rejected (0A000). A composite column is storable but never
            // keyable — the PK gate below rejects it 0A000 (§6).
            let (ty, decimal): (Type, Option<DecimalTypmod>) = if let Some(sk) = serial_kind {
                // A serial column takes no typmod (`serial(5)` is 42601) and no `[]` (handled by
                // the array branch). Its type is the underlying integer; everything else below.
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!("type modifier is not allowed for type {}", def.type_name),
                    ));
                }
                (Type::Scalar(sk), None)
            } else if let Some(base) = def.type_name.strip_suffix("[]") {
                // An array column (spec/design/array.md §3). The element type is a scalar or a
                // previously-defined composite (array-of-composite, §12 AC1 — `element_type_code`
                // 14 + name); a nested-array element and an array typmod (`numeric(p,s)[]`) stay
                // deferred (0A000).
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on an array type is not supported yet".to_string(),
                    ));
                }
                match ScalarType::from_name(base) {
                    Some(s) => (Type::Array(Box::new(Type::Scalar(s))), None),
                    None => {
                        if let Some(ctype) = self.read_snap().composite_type(base) {
                            (
                                Type::Array(Box::new(Type::Composite(
                                    crate::types::CompositeRef {
                                        name: ctype.name.clone(),
                                    },
                                ))),
                                None,
                            )
                        } else {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("type does not exist: {base}"),
                            ));
                        }
                    }
                }
            } else if ScalarType::from_name(&def.type_name).is_some() {
                let (s, d) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
                (Type::Scalar(s), d)
            } else if let Some(ctype) = self.read_snap().composite_type(&def.type_name) {
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a type modifier is not supported for composite type {}",
                            def.type_name
                        ),
                    ));
                }
                (
                    Type::Composite(crate::types::CompositeRef {
                        name: ctype.name.clone(),
                    }),
                    None,
                )
            } else {
                return Err(EngineError::new(
                    SqlState::UndefinedObject,
                    format!("type does not exist: {}", def.type_name),
                ));
            };
            if def.primary_key {
                // Integers, boolean, and uuid may be a key. uuid is the first non-integer key
                // type (fixed `uuid-raw16`, spec/design/encoding.md §2.7) and boolean the second
                // (fixed 1-byte `bool-byte`, §2.9) — both exercised + byte-pinned. The remaining
                // non-integer types' order-preserving key encodings (text §2.4, decimal §2.5,
                // bytea §2.6, interval, float §2.8) are authored but unexercised, so a
                // text/decimal/bytea/interval/float PRIMARY KEY is a documented 0A000 narrowing
                // (spec/design/types.md §11/§12/§13), relaxable in a later in-key slice.
                // timestamp / timestamptz are also allowed — they share the i64 `int-be-signflip`
                // key encoding (exercised + byte-pinned, spec/design/timestamp.md §6). date is
                // likewise allowed — the i32 `int-be-signflip` key encoding (spec/design/date.md §5).
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                if pk_seen {
                    return Err(EngineError::new(
                        SqlState::InvalidTableDefinition,
                        format!(
                            "multiple primary keys for table {} are not allowed",
                            ct.name
                        ),
                    ));
                }
                pk_seen = true;
                pk.push(columns.len()); // this column's ordinal (pushed below)
            }
            // Classify the DEFAULT by syntactic form (constraints.md §2). A bad default fails
            // at CREATE TABLE either way; NOT NULL is NOT enforced here (not_null=false), so a
            // `DEFAULT NULL` on a NOT NULL column is accepted and traps 23502 only when applied.
            //   - a bare literal is pre-evaluated + type-coerced to a constant value (the
            //     fast-path: out of range 22003, cross-family 42804, decimal rounded to typmod);
            //   - any other expression is validated (structural pre-walk, then resolved against
            //     an EMPTY scope — a default may not reference a column — then its result type is
            //     checked assignable to the column, 42804) and stored as text for per-row eval.
            let (default, default_expr) = if serial_kind.is_some() {
                // serial desugaring (sequences.md §12): an explicit DEFAULT conflicts with the
                // synthesized one (PG: "multiple default values specified", 42601). Otherwise create
                // the OWNED sequence and synthesize `DEFAULT nextval('<auto-name>')` — an ordinary
                // expression default (the format_version 8 mechanism), evaluated per row at INSERT.
                if def.default.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "multiple default values specified for column {} of table {}",
                            def.name, ct.name
                        ),
                    ));
                }
                let seqname = self.choose_serial_seq_name(&ct.name, &def.name, &pending_serials);
                let (min, max) = SequenceDef::default_bounds(1);
                pending_serials.push(SequenceDef {
                    name: seqname.clone(),
                    increment: 1,
                    min_value: min,
                    max_value: max,
                    start: min,
                    cache: 1,
                    cycle: false,
                    last_value: min,
                    is_called: false,
                    owned_by: Some(crate::catalog::SeqOwner {
                        table: ct.name.clone(),
                        column: columns.len() as u16, // this column's ordinal (pushed below)
                    }),
                });
                // Build the synthetic default exactly as the parser would render the equivalent
                // `DEFAULT nextval('<seqname>')` (space-joined tokens — the canonical expression-text
                // form), so the in-memory expr matches what reload re-parses (constraints.md §2). The
                // seqname is a lowercased identifier-derived name, so the quoting is always safe.
                let expr_text = format!("nextval ( '{}' )", seqname.replace('\'', "''"));
                let expr = crate::parser::parse_expression(&expr_text)?;
                (None, Some(DefaultExpr { expr_text, expr }))
            } else if ty.is_composite() || ty.is_array() {
                // A DEFAULT on a composite- or array-typed column is not supported this slice
                // (composite.md §12 / array.md §12).
                if def.default.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a DEFAULT on a composite- or array-typed column is not supported yet"
                            .to_string(),
                    ));
                }
                (None, None)
            } else {
                let sty = ty.scalar();
                match &def.default {
                    None => (None, None),
                    Some(d) => match &d.expr {
                        Expr::Literal(lit) => (
                            Some(store_value(
                                literal_to_value_for(lit, sty)?,
                                sty,
                                decimal,
                                false,
                                &def.name,
                            )?),
                            None,
                        ),
                        _ => {
                            reject_default_structure(&d.expr)?;
                            let scope = Scope::empty(self);
                            let (_, rty) = resolve(
                                &scope,
                                &d.expr,
                                Some(sty),
                                &mut AggCtx::Forbidden,
                                &mut ParamTypes::default(),
                            )?;
                            if !rty.assignable_to(sty) {
                                return Err(type_error(format!(
                                    "column {} is of type {} but default expression is of type {}",
                                    def.name,
                                    sty.canonical_name(),
                                    rty.type_name(),
                                )));
                            }
                            (
                                None,
                                Some(DefaultExpr {
                                    expr_text: d.text.clone(),
                                    expr: d.expr.clone(),
                                }),
                            )
                        }
                    },
                }
            };
            columns.push(Column {
                name: def.name.clone(),
                ty,
                decimal,
                primary_key: def.primary_key,
                // PRIMARY KEY ⇒ NOT NULL; a `serial` column is NOT NULL too (sequences.md §12).
                not_null: def.primary_key || def.not_null || serial_kind.is_some(),
                default,
                default_expr,
            });
        }

        // Table-level `PRIMARY KEY (a, b, …)` constraints (constraints.md §3). Check order
        // mirrors PostgreSQL (oracle-probed): a second primary key is 42P16 before its
        // members resolve; members resolve left to right (42703 unknown, 42701 repeated).
        // The LIST order is the KEY order — it may differ from declaration order (the v5
        // catalog persists the ordinal list; the old 0A000 narrowing is lifted). The
        // per-member key-type gate (0A000) remains.
        for pk_list in &ct.table_pks {
            if pk_seen {
                return Err(EngineError::new(
                    SqlState::InvalidTableDefinition,
                    format!(
                        "multiple primary keys for table {} are not allowed",
                        ct.name
                    ),
                ));
            }
            pk_seen = true;
            let mut indices: Vec<usize> = Vec::with_capacity(pk_list.len());
            for name in pk_list {
                let idx = columns
                    .iter()
                    .position(|c: &Column| c.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {name} named in key does not exist"),
                        )
                    })?;
                if indices.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {name} appears twice in primary key constraint"),
                    ));
                }
                indices.push(idx);
            }
            for &i in &indices {
                let ty = &columns[i].ty;
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                columns[i].primary_key = true;
                columns[i].not_null = true; // PRIMARY KEY ⇒ NOT NULL, per member
            }
            pk = indices;
        }

        // UNIQUE constraints (constraints.md §5.1): resolve members in textual definition
        // order, AFTER the PRIMARY KEY constraints and BEFORE any CHECK validates (PG's
        // order, oracle-probed — transformIndexConstraint runs first). Each member must
        // exist (42703, PG's "named in key" wording), appear once (42701), and be of a
        // key-encodable type (0A000 — the same narrowing as a PK member / index key column;
        // unlike a PK member it stays nullable). Folding + naming happen LAST (after check
        // naming), mirroring PG's index_create-at-execution timing.
        let mut runiques: Vec<(Option<String>, Vec<usize>)> = Vec::with_capacity(ct.uniques.len());
        for u in &ct.uniques {
            let mut indices: Vec<usize> = Vec::with_capacity(u.columns.len());
            for cname in &u.columns {
                let idx = columns
                    .iter()
                    .position(|c: &Column| c.name.eq_ignore_ascii_case(cname))
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {cname} named in key does not exist"),
                        )
                    })?;
                if indices.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {cname} appears twice in unique constraint"),
                    ));
                }
                indices.push(idx);
            }
            for &i in &indices {
                let ty = &columns[i].ty;
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a {} unique constraint member is not supported yet",
                            ty.canonical_name()
                        ),
                    ));
                }
            }
            runiques.push((u.name.clone(), indices));
        }

        // CHECK constraints (constraints.md §4). All validation runs first, in textual
        // definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
        // oracle-probed); naming follows in a second pass, so a 42703 in a later check
        // fires before a 42710 between earlier ones. Resolution needs a catalog `Table`,
        // so build it now (checks attach below, before `put_table`).
        let mut table = Table {
            name: ct.name,
            columns,
            pk,
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        };
        for def in &ct.checks {
            // Structural rejections first (a single pre-walk — a documented micro-order
            // divergence from PG, which interleaves them with name/type resolution):
            // subquery 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
            reject_check_structure(&def.expr)?;
            let scope = Scope::single(self, &table);
            let (_, ty) = resolve(
                &scope,
                &def.expr,
                None,
                &mut AggCtx::Forbidden,
                &mut ParamTypes::default(),
            )?;
            match ty {
                ResolvedType::Bool | ResolvedType::Null => {}
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
                | ResolvedType::Composite(_)
                | ResolvedType::Array(_) => {
                    return Err(type_error("argument of CHECK must be boolean"));
                }
            }
        }
        // Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
        // used as written; a derived name is built from the LOWERCASED table/column names —
        // `<table>_<col>_check` when the expression references exactly one distinct column,
        // else `<table>_check` — suffixed with the smallest positive integer that frees it.
        // A collision (case-insensitive, PG folds) is 42710; derived names never yield to a
        // later explicit one (oracle-probed).
        let mut checks: Vec<CheckConstraint> = Vec::with_capacity(ct.checks.len());
        for def in &ct.checks {
            let name = match &def.name {
                Some(n) => {
                    if checks.iter().any(|c| c.name.eq_ignore_ascii_case(n)) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let cols = check_referenced_columns(&def.expr, &table.columns);
                    let base = match cols.as_slice() {
                        [i] => format!(
                            "{}_{}_check",
                            table.name.to_ascii_lowercase(),
                            table.columns[*i].name.to_ascii_lowercase()
                        ),
                        _ => format!("{}_check", table.name.to_ascii_lowercase()),
                    };
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while checks
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            checks.push(CheckConstraint {
                name,
                expr_text: def.text.clone(),
                expr: def.expr.clone(),
            });
        }
        // Evaluation (and on-disk) order: ascending byte order of the lowercased name
        // (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
        checks.sort_by_key(|c| c.name.to_ascii_lowercase());
        table.checks = checks;

        // UNIQUE fold + naming (constraints.md §5.2/§5.3, PG-probed). Fold first: a
        // constraint whose member list equals the primary key's (same order) creates
        // nothing; identical lists fold into the first occurrence, the surviving name being
        // the first explicitly-named one's. Then each survivor names its backing index in
        // textual order: an explicit name checks the relation namespace (42P07 — existing
        // relations, the table being created, and the statement's earlier indexes) before
        // the table's constraint names (42710); a derived `<table>_<cols>_key` suffix-walks
        // past BOTH namespaces.
        let mut survivors: Vec<(Option<String>, Vec<usize>)> = Vec::new();
        for (uname, cols) in runiques {
            if cols == table.pk {
                continue;
            }
            if let Some(existing) = survivors.iter_mut().find(|(_, c)| *c == cols) {
                if existing.0.is_none() {
                    existing.0 = uname;
                }
                continue;
            }
            survivors.push((uname, cols));
        }
        for (uname, cols) in survivors {
            let taken = |exec: &Self, t: &Table, n: &str| {
                exec.relation_exists(n)
                    || t.name.eq_ignore_ascii_case(n)
                    || t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n))
            };
            let name = match uname {
                Some(n) => {
                    if taken(self, &table, &n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateTable,
                            format!("relation already exists: {n}"),
                        ));
                    }
                    if table.checks.iter().any(|c| c.name.eq_ignore_ascii_case(&n)) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n
                }
                None => {
                    let mut base = table.name.to_ascii_lowercase();
                    for &i in &cols {
                        base.push('_');
                        base.push_str(&table.columns[i].name.to_ascii_lowercase());
                    }
                    base.push_str("_key");
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while taken(self, &table, &candidate)
                        || table
                            .checks
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            // Insert in catalog (ascending lowercased-name) order — indexes.md §6.
            let name_key = name.to_ascii_lowercase();
            let pos = table
                .indexes
                .iter()
                .position(|i| i.name.to_ascii_lowercase() > name_key)
                .unwrap_or(table.indexes.len());
            table.indexes.insert(
                pos,
                IndexDef {
                    name,
                    columns: cols,
                    unique: true,
                    kind: IndexKind::Btree,
                },
            );
        }

        // FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
        // constraints (PG's order), each in textual definition order: resolve the local columns
        // (42703/42701) against this table; look up the parent (42P01, or the table itself for a
        // self-reference); resolve the referenced columns (default to the parent PK, 42830 if it
        // has none); check the arity (42830); name the constraint (explicit collision 42710, else
        // derive `<table>_<cols>_fkey` with a suffix walk through the constraint namespace);
        // reject the unsupported write-actions (0A000); require the referenced columns to be the
        // parent PK or a UNIQUE set (42830); and require same-type pairing (42804, stricter than
        // PG). An FK owns no B-tree — enforcement probes the parent at every write (§6.4/§6.5).
        let mut resolved_fks: Vec<ForeignKeyConstraint> = Vec::with_capacity(ct.foreign_keys.len());
        for fk in &ct.foreign_keys {
            // 1. Local (referencing) columns into this table.
            let mut local: Vec<usize> = Vec::with_capacity(fk.columns.len());
            for cname in &fk.columns {
                let idx = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(cname))
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {cname} named in key does not exist"),
                        )
                    })?;
                if local.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {cname} appears twice in foreign key constraint"),
                    ));
                }
                local.push(idx);
            }
            // 2. Parent table — a self-reference resolves against the in-progress definition.
            let self_ref = fk.ref_table.eq_ignore_ascii_case(&table.name);
            let parent: &Table = if self_ref {
                &table
            } else {
                self.table(&fk.ref_table).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedTable,
                        format!("table does not exist: {}", fk.ref_table),
                    )
                })?
            };
            // 3. Referenced columns into the parent (default to the parent's primary key).
            let refs: Vec<usize> = match &fk.ref_columns {
                None => {
                    if parent.pk.is_empty() {
                        // Omitting the referenced list defaults to the parent's PRIMARY KEY; a
                        // parent without one is 42704 (PG's code here — undefined_object — even
                        // when the parent has a UNIQUE), distinct from the explicit-no-match 42830.
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!(
                                "there is no primary key for referenced table {}",
                                parent.name
                            ),
                        ));
                    }
                    parent.pk.clone()
                }
                Some(cols) => {
                    let mut r: Vec<usize> = Vec::with_capacity(cols.len());
                    for cname in cols {
                        let idx = parent
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(cname))
                            .ok_or_else(|| {
                                EngineError::new(
                                    SqlState::UndefinedColumn,
                                    format!("column {cname} named in key does not exist"),
                                )
                            })?;
                        if r.contains(&idx) {
                            return Err(EngineError::new(
                                SqlState::DuplicateColumn,
                                format!("column {cname} appears twice in foreign key constraint"),
                            ));
                        }
                        r.push(idx);
                    }
                    r
                }
            };
            // 4. Referencing/referenced count must agree.
            if local.len() != refs.len() {
                return Err(EngineError::new(
                    SqlState::InvalidForeignKey,
                    "number of referencing and referenced columns for foreign key disagree"
                        .to_string(),
                ));
            }
            // 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
            let name = match &fk.name {
                Some(n) => {
                    if table.checks.iter().any(|c| c.name.eq_ignore_ascii_case(n))
                        || resolved_fks.iter().any(|f| f.name.eq_ignore_ascii_case(n))
                    {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let mut base = table.name.to_ascii_lowercase();
                    for &i in &local {
                        base.push('_');
                        base.push_str(&table.columns[i].name.to_ascii_lowercase());
                    }
                    base.push_str("_fkey");
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while table
                        .checks
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                        || resolved_fks
                            .iter()
                            .any(|f| f.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            // 6. Reject the unsupported write-actions (§6.6).
            let on_delete = fk_action(fk.on_delete, "DELETE")?;
            let on_update = fk_action(fk.on_update, "UPDATE")?;
            // 7. The referenced columns must be the parent's PK or a UNIQUE set (§6.2).
            let ref_set = sorted_unique(&refs);
            let matches_unique = (!parent.pk.is_empty() && sorted_unique(&parent.pk) == ref_set)
                || parent
                    .indexes
                    .iter()
                    .any(|i| i.unique && sorted_unique(&i.columns) == ref_set);
            if !matches_unique {
                return Err(EngineError::new(
                    SqlState::InvalidForeignKey,
                    format!(
                        "there is no unique constraint matching given keys for referenced table {}",
                        parent.name
                    ),
                ));
            }
            // 8. Same-type pairing (§6.2). Because the referenced columns are a PK/UNIQUE key they
            // are key-encodable, so a same-typed local column is key-encodable too — no separate
            // 0A000 type gate is needed.
            for (li, ri) in local.iter().zip(&refs) {
                if table.columns[*li].ty != parent.columns[*ri].ty {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!(
                            "foreign key constraint {name} cannot be implemented: key columns {} and {} are of incompatible types: {} and {}",
                            table.columns[*li].name,
                            parent.columns[*ri].name,
                            table.columns[*li].ty.canonical_name(),
                            parent.columns[*ri].ty.canonical_name(),
                        ),
                    ));
                }
            }
            resolved_fks.push(ForeignKeyConstraint {
                name,
                columns: local,
                ref_table: parent.name.clone(),
                ref_columns: refs,
                on_delete,
                on_update,
            });
        }
        // Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
        resolved_fks.sort_by_key(|f| f.name.to_ascii_lowercase());
        table.foreign_keys = resolved_fks;

        let index_keys: Vec<String> = table
            .indexes
            .iter()
            .map(|i| i.name.to_ascii_lowercase())
            .collect();
        self.put_table(table);
        // The table is brand new (no rows), so each backing index store starts empty.
        let cap = self.page_size as usize - 12; // PAGE_HEADER
        for k in index_keys {
            self.working_mut()
                .put_index_store(k, TableStore::new(cap, Vec::new()));
        }
        // Stage each `serial` column's OWNED sequence now that the table validated
        // (spec/design/sequences.md §12). The names were resolved (collision-free) during the column
        // walk; the table is in the catalog, so a `DROP TABLE` will auto-drop these.
        for s in pending_serials {
            self.working_mut().put_sequence(s);
        }
        // DDL touches no rows and evaluates no expressions: zero cost.
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Resolve a table's CHECK constraints for a write statement: each stored expression
    /// against a one-relation scope, in the catalog's (evaluation/name) order. Cannot fail
    /// for a catalog produced by CREATE TABLE or a well-formed file (both validated); a
    /// hand-corrupted expression surfaces its natural resolve error.
    fn resolve_checks(&self, table: &Table) -> Result<Vec<(String, RExpr)>> {
        if table.checks.is_empty() {
            return Ok(Vec::new());
        }
        let scope = Scope::single(self, table);
        let mut out = Vec::with_capacity(table.checks.len());
        for c in &table.checks {
            let (node, _) = resolve(
                &scope,
                &c.expr,
                None,
                &mut AggCtx::Forbidden,
                &mut ParamTypes::default(),
            )?;
            out.push((c.name.clone(), node));
        }
        Ok(out)
    }

    /// Resolve each column's **expression** `DEFAULT` (constraints.md §2) to an `RExpr`, once
    /// per INSERT statement — `insert_rows` (and the VALUES `DEFAULT`-keyword materialization)
    /// evaluate it per omitted/`DEFAULT` slot. Returns a slot per column (parallel to
    /// `table.columns`): `Some(node)` for an expression default, `None` for a column with a
    /// constant default or no default. The default resolves against an EMPTY scope (no columns;
    /// a column reference was rejected 0A000 at CREATE TABLE) with the column's type as the
    /// adaptable-operand hint.
    fn resolve_default_exprs(&self, table: &Table) -> Result<Vec<Option<RExpr>>> {
        let mut out = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            match &col.default_expr {
                Some(de) => {
                    let scope = Scope::empty(self);
                    let (node, _) = resolve(
                        &scope,
                        &de.expr,
                        Some(col.ty.scalar()),
                        &mut AggCtx::Forbidden,
                        &mut ParamTypes::default(),
                    )?;
                    out.push(Some(node));
                }
                None => out.push(None),
            }
        }
        Ok(out)
    }

    /// The value an omitted column or a `DEFAULT` value slot takes (constraints.md §2): the
    /// column's pre-evaluated constant (`col.default`, or NULL when it has none), OR — for an
    /// expression default — the resolved `RExpr` evaluated against an empty row through the
    /// per-statement seam/clock (`rng`) and metered (`operator_eval` per node). Reused by the
    /// VALUES materialization (a `DEFAULT` keyword) and `insert_rows` (an omitted column),
    /// sharing ONE `StmtRng` so a multi-row `DEFAULT uuidv7()` stays monotonic.
    fn eval_default(
        &self,
        col: &Column,
        default_rexpr: Option<&RExpr>,
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
    ) -> Result<Value> {
        match default_rexpr {
            Some(rx) => {
                meter.guard()?;
                let env = EvalEnv {
                    exec: self,
                    params: &[],
                    outer: &[],
                    rng,
                    ctes: CteCtx::empty(),
                };
                rx.eval(&[], &env, meter)
            }
            None => Ok(col.default.clone().unwrap_or(Value::Null)),
        }
    }

    /// Run a DROP TABLE: remove the table's definition and its row store from the
    /// catalog (both keyed by the lower-cased name). A table that does not exist is the
    /// same 42P01 the DML paths raise — there is no `IF EXISTS` this slice
    /// (spec/design/grammar.md §13). Like CREATE TABLE it touches no rows and evaluates
    /// no expression tree (the store is discarded wholesale), so it accrues zero cost.
    fn execute_drop_table(&mut self, dt: DropTable) -> Result<Outcome> {
        if self.table(&dt.name).is_none() {
            // An index's name is the wrong object kind (42809 — indexes.md §2, PG-probed);
            // anything else is the missing-table 42P01 the DML paths raise.
            if self.find_index(&dt.name).is_some() {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("{} is not a table", dt.name),
                ));
            }
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", dt.name),
            ));
        }
        // A table referenced by ANOTHER table's FOREIGN KEY cannot be dropped (2BP01 — there is no
        // DROP TABLE … CASCADE; a self-reference does not block — spec/design/constraints.md §6.10).
        if let Some(detail) = self.read_snap().foreign_key_dependent(&dt.name) {
            let canonical = self
                .table(&dt.name)
                .map_or(dt.name.clone(), |t| t.name.clone());
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop table {canonical} because other objects depend on it: {detail}"
                ),
            ));
        }
        // Auto-drop every sequence OWNED BY this table — the `serial` columns' sequences
        // (spec/design/sequences.md §12). An owned sequence is never an FK dependent, so the check
        // above never blocked on it; the sequences are removed alongside the table.
        let owned_seqs = self.read_snap().sequences_owned_by(&dt.name);
        let key = dt.name.to_ascii_lowercase();
        let w = self.working_mut();
        for sk in &owned_seqs {
            w.remove_sequence(sk);
        }
        w.remove_table(&key);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE INDEX (spec/design/indexes.md §2). Validation mirrors
    /// PostgreSQL's order (oracle-probed): the table must exist (42P01); each key column, in
    /// list order, must exist (42703) and be of a key-encodable type (0A000 — the same
    /// narrowing as a PRIMARY KEY member); then an explicit name is checked against the
    /// shared relation namespace (42P07), or an omitted name derives PG's choice — the
    /// lowercased `<table>_<col>..._idx` with the smallest free suffix. The index is then
    /// built by scanning the table once: `page_read` per node + `storage_row_read` per row
    /// (the metered build scan — cost.md §3); maintenance thereafter is unmetered.
    fn execute_create_index(&mut self, ci: CreateIndex) -> Result<Outcome> {
        let table = self.table(&ci.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", ci.table),
            )
        })?;
        let table_key = table.name.to_ascii_lowercase();
        let columns = table.columns.clone();
        // Resolve the access method (spec/design/gin.md §3): the default / `btree` is the ordered
        // B-tree, `gin` a GIN inverted index; an unknown method is 42704. Resolved here (not in the
        // parser) so the error is the resolve-time undefined_object, after the table-exists check
        // and before the column checks.
        let kind = match ci.using.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("btree") => IndexKind::Btree,
            Some("gin") => IndexKind::Gin,
            Some(other) => {
                return Err(EngineError::new(
                    SqlState::UndefinedObject,
                    format!("access method does not exist: {other}"),
                ));
            }
        };
        let mut cols: Vec<usize> = Vec::with_capacity(ci.columns.len());
        for name in &ci.columns {
            let idx = table.column_index(name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column does not exist: {name}"),
                )
            })?;
            let ty = &columns[idx].ty;
            match kind {
                IndexKind::Btree => {
                    if !ty.is_integer()
                        && !ty.is_bool()
                        && !ty.is_uuid()
                        && !ty.is_timestamp()
                        && !ty.is_timestamptz()
                        && !ty.is_date()
                    {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a {} index column is not supported yet",
                                ty.canonical_name()
                            ),
                        ));
                    }
                }
                IndexKind::Gin => {
                    // GIN needs an operator class for the column type: only an array has one (else
                    // 42704, no default opclass), and this slice only the integer element types
                    // (else 0A000) — spec/design/gin.md §3.
                    match ty.array_element() {
                        None => {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!(
                                    "data type {} has no default operator class for access method gin",
                                    ty.canonical_name()
                                ),
                            ));
                        }
                        Some(elem) if !elem.is_integer() => {
                            return Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                format!(
                                    "a gin index on {} is not supported yet",
                                    ty.canonical_name()
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
            // A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
            cols.push(idx);
        }
        // GIN narrowings this slice (spec/design/gin.md §3): no uniqueness (undefined for an
        // inverted index) and a single column only — both deferred 0A000.
        if kind == IndexKind::Gin {
            if ci.unique {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "access method gin does not support unique indexes".to_string(),
                ));
            }
            if cols.len() != 1 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a multi-column gin index is not supported yet".to_string(),
                ));
            }
        }
        let name = match &ci.name {
            Some(n) => {
                if self.relation_exists(n) {
                    return Err(EngineError::new(
                        SqlState::DuplicateTable,
                        format!("relation already exists: {n}"),
                    ));
                }
                n.clone()
            }
            None => {
                // PG's ChooseIndexName (probed): lowercased table + every listed column name
                // (list order, duplicates included) + "idx", then the smallest free suffix.
                let mut base = table_key.clone();
                for name in &ci.columns {
                    base.push('_');
                    base.push_str(&name.to_ascii_lowercase());
                }
                base.push_str("_idx");
                let mut candidate = base.clone();
                let mut suffix = 0u32;
                while self.relation_exists(&candidate) {
                    suffix += 1;
                    candidate = format!("{base}{suffix}");
                }
                candidate
            }
        };

        // The build scan (cost.md §3): page_read per table-tree node + storage_row_read per
        // row, with the indexed columns as the touched set (fixed-width — the chain/decompress
        // terms are structurally zero). An empty table charges 0. The entries are computed
        // here, against the pre-index store; the writes below are unmetered.
        let mut meter = Meter::with_limit(self.max_cost);
        let mut mask = vec![false; columns.len()];
        for &c in &cols {
            mask[c] = true;
        }
        let def = IndexDef {
            name,
            columns: cols,
            unique: ci.unique,
            kind,
        };
        let store = self.store(&ci.table);
        let (table_entries, nodes, slabs) = store.scan_with_units(&mask)?;
        meter.charge(COSTS.page_read * nodes as i64 + COSTS.value_decompress * slabs as i64);
        let mut entries: Vec<Vec<u8>> = Vec::with_capacity(store.len());
        // A UNIQUE build verifies the existing rows before the index is registered
        // (indexes.md §8): two rows sharing a fully-non-NULL key tuple — i.e. an exempt-free
        // prefix — trap 23505 and create nothing. Unmetered validation (cost.md §3).
        let mut seen_prefixes: HashSet<Vec<u8>> = HashSet::new();
        for (key, row) in table_entries {
            meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
            meter.charge(COSTS.storage_row_read);
            if def.unique
                && let Some(prefix) = index_prefix_key(&columns, &def, &row)
                && !seen_prefixes.insert(prefix)
            {
                return Err(EngineError::new(
                    SqlState::UniqueViolation,
                    format!(
                        "duplicate key value violates unique constraint: {}",
                        def.name
                    ),
                ));
            }
            entries.extend(index_entry_keys(&columns, &def, &key, &row));
        }
        meter.guard()?;

        let name_key = def.name.to_ascii_lowercase();
        let ps = self.page_size;
        self.working_mut().put_index(&table_key, def, ps);
        let istore = self.index_store_mut(&name_key);
        // Insert sorted by entry key (indexes.md §1): every insert is then a right-edge append,
        // so the built tree packs ~full instead of splintering under the storage-key order the
        // scan produced (random in entry-key space). Part of the byte contract — the sort fixes
        // the built tree's shape across cores.
        entries.sort_unstable();
        for ek in entries {
            assert!(
                istore.insert(ek, Vec::new())?,
                "index entry keys are unique (storage-key suffix)"
            );
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: None,
        })
    }

    /// Run a DROP INDEX (spec/design/indexes.md §2): a table's name is 42809, a missing one
    /// 42704. A pure catalog edit — zero cost, like DROP TABLE.
    fn execute_drop_index(&mut self, di: DropIndex) -> Result<Outcome> {
        if self.table(&di.name).is_some() {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!("{} is not an index", di.name),
            ));
        }
        let Some((table_key, _)) = self.find_index(&di.name) else {
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("index does not exist: {}", di.name),
            ));
        };
        let table_key = table_key.to_string();
        let name_key = di.name.to_ascii_lowercase();
        self.working_mut().remove_index(&table_key, &name_key);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE TYPE (spec/design/composite.md): reject a duplicate type name
    /// (42710), resolve each field's type (a built-in scalar, or a *previously-defined* composite
    /// — 42704 if unknown; no self- or forward-reference), reject a duplicate field name (42701),
    /// then register the composite type in the catalog. Named composites only.
    fn execute_create_type(&mut self, ct: CreateType) -> Result<Outcome> {
        if self.read_snap().composite_type(&ct.name).is_some() {
            return Err(EngineError::new(
                SqlState::DuplicateObject,
                format!("type {} already exists", ct.name),
            ));
        }
        let mut fields: Vec<CompositeField> = Vec::with_capacity(ct.fields.len());
        for f in &ct.fields {
            if fields.iter().any(|g| g.name.eq_ignore_ascii_case(&f.name)) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("attribute {} specified more than once", f.name),
                ));
            }
            let (fty, fdecimal): (Type, Option<DecimalTypmod>) =
                if let Some(base) = f.type_name.strip_suffix("[]") {
                    // An array-typed field (spec/design/array.md §12 — the mirror of an
                    // array-of-composite element). The element is a scalar or a *previously-defined*
                    // composite (`element_type_code` 14 + name on disk); a nested-array element and
                    // an array typmod (`numeric(p,s)[]`) stay deferred (0A000), exactly as for an
                    // array column.
                    if f.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a type modifier on an array type is not supported yet".to_string(),
                        ));
                    }
                    let elem = if let Some(s) = ScalarType::from_name(base) {
                        Type::Scalar(s)
                    } else if let Some(ctype) = self.read_snap().composite_type(base) {
                        Type::Composite(crate::types::CompositeRef {
                            name: ctype.name.clone(),
                        })
                    } else {
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!("type does not exist: {base}"),
                        ));
                    };
                    (Type::Array(Box::new(elem)), None)
                } else if ScalarType::from_name(&f.type_name).is_some() {
                    let (s, d) = resolve_type_and_typmod(&f.type_name, &f.type_mod)?;
                    (Type::Scalar(s), d)
                } else if self.read_snap().composite_type(&f.type_name).is_some() {
                    if f.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a type modifier is not supported for composite type {}",
                                f.type_name
                            ),
                        ));
                    }
                    (
                        Type::Composite(crate::types::CompositeRef {
                            name: f.type_name.clone(),
                        }),
                        None,
                    )
                } else {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("type does not exist: {}", f.type_name),
                    ));
                };
            fields.push(CompositeField {
                name: f.name.clone(),
                ty: fty,
                decimal: fdecimal,
                not_null: f.not_null,
            });
        }
        // Bound composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A chain of CREATE TYPEs
        // each nesting the previous (`a`, `b AS (x a)`, …) builds unbounded depth across many cheap
        // statements — invisible to the per-statement input-size cap and the parser nesting counter —
        // and every derived recursive walk (codec, comparator, record_out/in, resolve_col_type)
        // recurses to this depth. Reject at the producer so no over-deep type enters the catalog and
        // every downstream walk stays stack-safe. Fields reference only existing types (each already
        // ≤ MAX_COMPOSITE_DEPTH), so this depth computation's recursion is itself bounded.
        let mut cache: HashMap<String, usize> = HashMap::new();
        let mut max_field = 0;
        for f in &fields {
            max_field = max_field.max(self.read_snap().composite_type_depth(&f.ty, &mut cache));
        }
        let depth = 1 + max_field;
        if depth > MAX_COMPOSITE_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "composite type {} nesting depth {depth} exceeds the maximum of {MAX_COMPOSITE_DEPTH}",
                    ct.name
                ),
            ));
        }
        self.working_mut().put_type(CompositeType {
            name: ct.name.clone(),
            fields,
        });
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a DROP TYPE (spec/design/composite.md §7). RESTRICT (the only behavior
    /// this slice): a missing type is 42704 unless `IF EXISTS`; if any table column or composite
    /// field still references the type, 2BP01; otherwise remove it from the catalog.
    fn execute_drop_type(&mut self, dt: DropType) -> Result<Outcome> {
        if self.read_snap().composite_type(&dt.name).is_none() {
            if dt.if_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("type does not exist: {}", dt.name),
            ));
        }
        if let Some(dep) = self.read_snap().composite_dependent(&dt.name) {
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop type {} because other objects depend on it: {}",
                    dt.name, dep
                ),
            ));
        }
        let key = dt.name.to_ascii_lowercase();
        self.working_mut().remove_type(&key);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE SEQUENCE (spec/design/sequences.md). Resolve the option overrides
    /// against the INCREMENT sign's type defaults, validate the set (22023), reject a relation-
    /// namespace collision (42P07 unless `IF NOT EXISTS`), and register the sequence.
    fn execute_create_sequence(&mut self, cs: CreateSequence) -> Result<Outcome> {
        if self.relation_exists(&cs.name) {
            if cs.if_not_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", cs.name),
            ));
        }
        let increment = cs.increment.unwrap_or(1);
        if increment == 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "INCREMENT must not be zero".to_string(),
            ));
        }
        let cache = cs.cache.unwrap_or(1);
        if cache < 1 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("CACHE ({cache}) must be greater than zero"),
            ));
        }
        let (def_min, def_max) = SequenceDef::default_bounds(increment);
        // `Some(Some(v))` MINVALUE v / `Some(None)` NO MINVALUE / `None` unset → the default.
        let min_value = match cs.min_value {
            Some(Some(v)) => v,
            Some(None) | None => def_min,
        };
        let max_value = match cs.max_value {
            Some(Some(v)) => v,
            Some(None) | None => def_max,
        };
        if min_value > max_value {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("MINVALUE ({min_value}) must be less than MAXVALUE ({max_value})"),
            ));
        }
        // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
        let start = cs
            .start
            .unwrap_or(if increment < 0 { max_value } else { min_value });
        if start < min_value || start > max_value {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!(
                    "START value ({start}) cannot be {} the {} value",
                    if start < min_value {
                        "less than MINVALUE"
                    } else {
                        "greater than MAXVALUE"
                    },
                    if start < min_value {
                        min_value
                    } else {
                        max_value
                    }
                ),
            ));
        }
        self.working_mut().put_sequence(SequenceDef {
            name: cs.name.clone(),
            increment,
            min_value,
            max_value,
            start,
            cache,
            cycle: cs.cycle.unwrap_or(false),
            last_value: start,
            is_called: false,
            owned_by: None,
        });
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a DROP SEQUENCE (spec/design/sequences.md §1). RESTRICT-only: a missing
    /// sequence is 42P01 unless `IF EXISTS`. No dependency tracking this slice (a plain `DEFAULT
    /// nextval('s')` creates none — PG). Multiple names are dropped left to right.
    fn execute_drop_sequence(&mut self, ds: DropSequence) -> Result<Outcome> {
        for name in &ds.names {
            // Missing → 42P01 (unless IF EXISTS). An OWNED (serial) sequence has a dependent — its
            // column's default — so RESTRICT (the only mode this slice; CASCADE 0A000) is 2BP01
            // (spec/design/sequences.md §12). Clone the owner ref out so the snapshot borrow ends
            // before the working-snapshot mutation.
            let owner = match self.read_snap().sequence(name) {
                None => {
                    if ds.if_exists {
                        continue;
                    }
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("sequence does not exist: {name}"),
                    ));
                }
                Some(s) => s
                    .owned_by
                    .as_ref()
                    .map(|o| (s.name.clone(), o.table.clone(), o.column)),
            };
            if let Some((seq_name, owner_table, owner_col)) = owner {
                // The owning table is always present (its own DROP TABLE would auto-drop this
                // sequence first), so the column name for the detail resolves.
                let (col_name, table_name) = self
                    .read_snap()
                    .table(&owner_table)
                    .map(|t| {
                        (
                            t.columns
                                .get(owner_col as usize)
                                .map_or_else(String::new, |c| c.name.clone()),
                            t.name.clone(),
                        )
                    })
                    .unwrap_or_else(|| (String::new(), owner_table.clone()));
                return Err(EngineError::new(
                    SqlState::DependentObjectsStillExist,
                    format!(
                        "cannot drop sequence {seq_name} because other objects depend on it: default value for column {col_name} of table {table_name} depends on sequence {seq_name}"
                    ),
                ));
            }
            let key = name.to_ascii_lowercase();
            self.working_mut().remove_sequence(&key);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run an `ALTER SEQUENCE [IF EXISTS] s RESTART [WITH n]` (spec/design/sequences.md
    /// §4). A missing sequence is 42P01 unless `IF EXISTS` (then a no-op). `RESTART WITH n` resets
    /// `last_value` to `n`, a bare `RESTART` to the original `START` (unchanged); either way
    /// `is_called = false`, so the next `nextval` returns that value. A restart value outside
    /// `[min_value, max_value]` is 22023. Touches no session state (`currval`/`lastval` unchanged).
    fn execute_alter_sequence(&mut self, als: AlterSequence) -> Result<Outcome> {
        let mut def = match self.read_snap().sequence(&als.name) {
            Some(d) => d.clone(),
            None => {
                if als.if_exists {
                    return Ok(Outcome::Statement {
                        cost: 0,
                        rows_affected: None,
                    });
                }
                return Err(EngineError::new(
                    SqlState::UndefinedTable,
                    format!("relation does not exist: {}", als.name),
                ));
            }
        };
        let value = als.restart_with.unwrap_or(def.start);
        if value < def.min_value || value > def.max_value {
            // PG's init_params path: 22023 (distinct from setval's 22003 do_setval path — §4).
            let bound = if value > def.max_value {
                format!("greater than MAXVALUE ({})", def.max_value)
            } else {
                format!("less than MINVALUE ({})", def.min_value)
            };
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("RESTART value ({value}) cannot be {bound}"),
            ));
        }
        def.last_value = value;
        def.is_called = false;
        self.working_mut().put_sequence(def);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
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
            returning,
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
        // The key members in key order — one for a single-column PK, several for a
        // composite (constraints.md §3), empty for a no-PK (rowid) table.
        let pk: Vec<(usize, ScalarType)> = tdef
            .pk_indices()
            .into_iter()
            .map(|i| (i, tdef.columns[i].ty.scalar()))
            .collect();
        // The CHECK constraints, resolved once per statement in evaluation (name) order;
        // `insert_rows` evaluates them per candidate row (constraints.md §4.4).
        let checks = self.resolve_checks(tdef)?;
        // Each column's EXPRESSION default, resolved once per statement (constraints.md §2);
        // applied per omitted column / `DEFAULT` slot, sharing one per-statement `StmtRng`.
        let default_exprs = self.resolve_default_exprs(tdef)?;
        // The columns' resolved `ColType`s (a scalar, or a composite resolved to its field tree),
        // for composite-aware materialization + store-coercion (spec/design/composite.md §4).
        let col_types: Vec<ColType> = self.store(&table).col_types().to_vec();
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());

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
                // checking each row's arity (42601) as it is visited, then bind the supplied
                // values up front so a bad bind fails before any store.
                let mut ptypes = ParamTypes::default();
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
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            // Only a scalar column gives a top-level `$N` an inferable type; a
                            // composite-column param stays untyped (42P18 at finalize this slice).
                            if let (Some(InsertValue::Param(nn)), Type::Scalar(s)) =
                                (values.get(p), &col.ty)
                            {
                                ptypes.note((*nn as usize) - 1, Some(*s))?;
                            }
                        }
                    }
                }
                // Resolve the RETURNING projection after the source (PostgreSQL's analysis
                // order) and before binding/execution — a 42703 here beats a would-be 23505
                // (grammar.md §32).
                let ret = match &returning {
                    Some(items) => {
                        Some(self.resolve_returning(&table, items, false, &mut ptypes)?)
                    }
                    None => None,
                };
                let bound = bind_params(params, &ptypes.finalize()?)?;

                // INSERT ... VALUES reads no rows; with only literal values and constant
                // defaults it evaluates no expression tree (leaves), so a plain fully-inline
                // insert still costs zero. An EXPRESSION default (`DEFAULT uuidv7()`) evaluates a
                // tree per application — `operator_eval` per node — the documented exception
                // (constraints.md §2, like CHECK). Other metered work: the disposition plan's
                // compression attempts for over-RECORD_MAX rows (value_compress) and the
                // RETURNING projection. The meter is created here (before materialization) so a
                // `DEFAULT`-keyword expression default charges it too.
                let mut meter = Meter::with_limit(self.max_cost);

                // Materialize each row into its value-position-indexed candidates (length
                // `arity`, checked above), resolving each slot: a literal, a bound `$N`, or a
                // `DEFAULT` keyword → that column's default (a constant, or its expression
                // evaluated for this row through the shared `stmt_rng`). The shared `insert_rows`
                // then builds the declaration-order row, applies any OMITTED defaults, and
                // validates it.
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rows_in.len());
                for values in &rows_in {
                    let mut rv = vec![Value::Null; arity];
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            rv[p] = match &values[p] {
                                // DEFAULT at the top level → the column's default (constant or
                                // per-row expression). A `ROW(…)` / literal / `$N` slot is
                                // materialized against the column's resolved type (composite-aware).
                                InsertValue::Default => self.eval_default(
                                    col,
                                    default_exprs[i].as_ref(),
                                    &stmt_rng,
                                    &mut meter,
                                )?,
                                other => materialize_insert_value(other, &col_types[i], &bound)?,
                            };
                        }
                    }
                    rows.push(rv);
                }
                let mut ret_nodes = ret;
                if let Some((nodes, _, _)) = &mut ret_nodes {
                    // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3),
                    // reading the pre-statement snapshot (grammar.md §32).
                    for node in nodes {
                        self.fold_uncorrelated_in_rexpr(
                            node,
                            &bound,
                            CteCtx::empty(),
                            &mut meter.accrued,
                        )?;
                    }
                }
                let inserted = rows.len() as i64;
                let returned = self.insert_rows(
                    &table,
                    &columns,
                    &pk,
                    &checks,
                    &default_exprs,
                    &stmt_rng,
                    &provided,
                    rows,
                    ret_nodes.as_ref().map(|(nodes, _, _)| nodes.as_slice()),
                    &bound,
                    &mut meter,
                )?;
                Ok(match (ret_nodes, returned) {
                    (Some((_, names, types)), Some(rows)) => Outcome::Query {
                        column_names: names,
                        column_types: types,
                        rows,
                        cost: meter.accrued,
                    },
                    _ => Outcome::Statement {
                        cost: meter.accrued,
                        rows_affected: Some(inserted),
                    },
                })
            }
            InsertSource::Select(sel) => {
                // Plan the source query, then resolve the RETURNING projection (PostgreSQL's
                // analysis order — both precede any execution), threading ONE ParamTypes so a
                // `$N` shared by the source and the RETURNING list unifies statement-wide
                // (api.md §5). The source returns OWNED rows, so the `&mut self` borrow ends
                // before phase 2 mutates the store (a self-insert reads the pre-insert
                // snapshot — §24).
                let mut ptypes = ParamTypes::default();
                let mut plan = self.plan_query(&QueryExpr::Select(sel), None, &[], &mut ptypes)?;
                let ret = match &returning {
                    Some(items) => {
                        Some(self.resolve_returning(&table, items, false, &mut ptypes)?)
                    }
                    None => None,
                };
                let bound = bind_params(params, &ptypes.finalize()?)?;
                let mut meter = Meter::with_limit(self.max_cost);
                self.fold_uncorrelated_in_plan(
                    &mut plan,
                    &bound,
                    CteCtx::empty(),
                    &mut meter.accrued,
                )?;
                let mut ret_nodes = ret;
                if let Some((nodes, _, _)) = &mut ret_nodes {
                    for node in nodes {
                        self.fold_uncorrelated_in_rexpr(
                            node,
                            &bound,
                            CteCtx::empty(),
                            &mut meter.accrued,
                        )?;
                    }
                }
                let q = self.exec_query_plan(&plan, &[], &bound, CteCtx::empty())?;

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
                        match &col.ty {
                            Type::Scalar(s) => {
                                if !q.column_types[p].assignable_to(*s) {
                                    return Err(type_error(format!(
                                        "column {} is of type {} but expression is of type {}",
                                        col.name,
                                        col.ty.canonical_name(),
                                        q.column_types[p].type_name(),
                                    )));
                                }
                            }
                            // INSERT ... SELECT into a composite column lands in a later slice
                            // (the VALUES + ROW(…) path is S3 — spec/design/composite.md §12).
                            Type::Composite(_) => {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "INSERT ... SELECT into composite column {} is not supported yet",
                                        col.name
                                    ),
                                ));
                            }
                            // INSERT ... SELECT into an array column is deferred (the VALUES +
                            // ARRAY[…] path is the supported input — spec/design/array.md §12).
                            Type::Array(_) => {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "INSERT ... SELECT into array column {} is not supported yet",
                                        col.name
                                    ),
                                ));
                            }
                        }
                    }
                }

                // Cost = the embedded SELECT's accrued cost (§24) plus the disposition plan's
                // compression attempts for over-RECORD_MAX rows (value_compress, cost.md §3)
                // plus the RETURNING projection; storing the rows themselves stays unmetered.
                // One meter keeps one ceiling over the whole statement.
                meter.charge(q.cost);
                let inserted = q.rows.len() as i64;
                let returned = self.insert_rows(
                    &table,
                    &columns,
                    &pk,
                    &checks,
                    &default_exprs,
                    &stmt_rng,
                    &provided,
                    q.rows,
                    ret_nodes.as_ref().map(|(nodes, _, _)| nodes.as_slice()),
                    &bound,
                    &mut meter,
                )?;
                Ok(match (ret_nodes, returned) {
                    (Some((_, names, types)), Some(rows)) => Outcome::Query {
                        column_names: names,
                        column_types: types,
                        rows,
                        cost: meter.accrued,
                    },
                    _ => Outcome::Statement {
                        cost: meter.accrued,
                        rows_affected: Some(inserted),
                    },
                })
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
    ///
    /// The argument list mirrors the statement-resolved inputs phase 1 validates against,
    /// one-for-one with the Go/TS cores — grouping them would only add indirection.
    ///
    /// `returning` is the resolved RETURNING projection (grammar.md §32), evaluated over the
    /// validated rows after every check passes and BEFORE phase 2 writes — so its subqueries
    /// observe the pre-statement snapshot and a ceiling abort stays all-or-nothing; `params`
    /// feeds its `$N`s. Returns the projected output rows, `None` without a clause.
    #[allow(clippy::too_many_arguments)]
    fn insert_rows(
        &mut self,
        table: &str,
        columns: &[Column],
        pk: &[(usize, ScalarType)],
        checks: &[(String, RExpr)],
        default_exprs: &[Option<RExpr>],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        provided: &[Option<usize>],
        rows: Vec<Vec<Value>>,
        returning: Option<&[RExpr]>,
        params: &[Value],
        meter: &mut Meter,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let n = columns.len();
        // The canonical relation name for the 23514 message (the `table` argument is the
        // name as the statement spelled it), and the index definitions phase 2 maintains
        // (spec/design/indexes.md §4 — unmetered write work, like the row writes).
        let (relation, indexes) = self
            .table(table)
            .map(|t| (t.name.clone(), t.indexes.clone()))
            .unwrap_or_else(|| (table.to_string(), Vec::new()));
        // The columns' resolved `ColType`s, for composite-aware store coercion (composite.md §4).
        let col_types: Vec<ColType> = self.store(table).col_types().to_vec();
        let mut prepared: Vec<(Option<Vec<u8>>, Row)> = Vec::with_capacity(rows.len());
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
        // Per UNIQUE index (catalog/name order), the prefixes earlier rows of this batch
        // claimed — an in-batch duplicate traps 23505 like a stored one (indexes.md §8).
        let uniq_defs: Vec<&IndexDef> = indexes.iter().filter(|d| d.unique).collect();
        let mut seen_prefixes: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); uniq_defs.len()];
        let mut cunits: i64 = 0;
        for values in &rows {
            let mut row = Vec::with_capacity(n);
            for (i, col) in columns.iter().enumerate() {
                let candidate = match provided[i] {
                    Some(p) => values[p].clone(),
                    // An omitted column takes its default — a constant, or its expression
                    // evaluated for this row through the shared per-statement seam/clock
                    // (constraints.md §2). `eval_default` charges `operator_eval` for an
                    // expression default; a constant (or no default → NULL) is free.
                    None => self.eval_default(col, default_exprs[i].as_ref(), rng, meter)?,
                };
                row.push(coerce_for_store(
                    candidate,
                    &col_types[i],
                    col.decimal,
                    col.not_null,
                    &col.name,
                )?);
            }

            // CHECK constraints, in name order, on the fully-coerced candidate row — after
            // NOT NULL (`store_value` above), before the key/duplicate check (PG's per-row
            // order, constraints.md §4.4). TRUE and NULL pass; the first FALSE aborts the
            // whole statement (two-phase — nothing has been written). Evaluation is metered
            // expression work (operator_eval), so guard the ceiling per checked row. The
            // per-statement `rng` is shared with the default evaluation above (one `StmtRng`).
            if !checks.is_empty() {
                meter.guard()?;
                let env = EvalEnv {
                    exec: self,
                    params: &[],
                    outer: &[],
                    rng,
                    ctes: CteCtx::empty(),
                };
                for (name, rexpr) in checks {
                    if matches!(rexpr.eval(&row, &env, meter)?, Value::Bool(false)) {
                        return Err(EngineError::new(
                            SqlState::CheckViolation,
                            format!(
                                "new row for relation {relation} violates check constraint {name}"
                            ),
                        ));
                    }
                }
            }

            let key = if pk.is_empty() {
                None
            } else {
                // The composite key is the concatenation of the members' bare encodings in
                // key order (encoding.md §2.3) — every keyable type is fixed-width, so the
                // concatenation is self-delimiting and memcmp equals the tuple's order. A
                // single-column key is the one-member case of the same rule.
                let mut k = Vec::new();
                for &(i, pk_ty) in pk {
                    match &row[i] {
                        Value::Int(nn) => k.extend_from_slice(&encode_int(pk_ty, *nn)),
                        // uuid is the first non-integer key: its key is the bare 16 bytes
                        // (uuid-raw16, encoding.md §2.7) — a PK is NOT NULL, so no presence tag.
                        Value::Uuid(u) => k.extend_from_slice(u),
                        // boolean is the second non-integer key: the bare 1-byte `bool-byte`
                        // (0x00 false / 0x01 true, encoding.md §2.9) — likewise no presence tag.
                        Value::Bool(b) => k.extend_from_slice(&encode_bool(*b)),
                        // A timestamp / timestamptz PRIMARY KEY is supported: its key bytes are
                        // the i64 instant codec (spec/design/timestamp.md §6).
                        Value::Timestamp(m) | Value::Timestamptz(m) => {
                            k.extend_from_slice(&encode_int(pk_ty, *m))
                        }
                        // A date PRIMARY KEY is supported: the i32 day codec (spec/design/date.md §5).
                        Value::Date(d) => k.extend_from_slice(&encode_int(pk_ty, *d as i64)),
                        // Unreachable: a PK column is NOT NULL, enforced above.
                        Value::Null => unreachable!("primary key column is NOT NULL"),
                        // Unreachable: a text/decimal/bytea/interval/float PRIMARY KEY is rejected
                        // at CREATE TABLE (0A000) — those non-integer PKs are caught by the gate.
                        Value::Text(_)
                        | Value::Decimal(_)
                        | Value::Bytea(_)
                        | Value::Interval(_)
                        | Value::Float32(_)
                        | Value::Float64(_) => {
                            unreachable!(
                                "a text/decimal/bytea/interval/float primary key is rejected at CREATE TABLE"
                            )
                        }
                        // Unreachable: a composite PRIMARY KEY is rejected at CREATE TABLE (0A000 —
                        // the key encoding is authored but unexercised, spec/design/composite.md §6).
                        Value::Composite(_) => {
                            unreachable!("a composite primary key is rejected at CREATE TABLE")
                        }
                        // Unreachable: an array PRIMARY KEY is rejected at CREATE TABLE (0A000 —
                        // the key encoding is authored but unexercised, spec/design/array.md §8).
                        Value::Array(_) => {
                            unreachable!("an array primary key is rejected at CREATE TABLE")
                        }
                        // Poisoned (large-values.md §14): INSERT values are evaluated
                        // expressions, never lazily-loaded storage rows.
                        Value::Unfetched(_) => {
                            panic!("BUG: unfetched large value escaped the storage layer")
                        }
                    }
                }
                if seen_keys.contains(&k) || self.store(table).get(&k)?.is_some() {
                    // The PK's 23505 reports PostgreSQL's derived auto-name for the PK
                    // index, `<table>_pkey` — jed persists/reserves no such relation
                    // (constraints.md §5.4).
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}_pkey",
                            relation.to_ascii_lowercase()
                        ),
                    ));
                }
                seen_keys.insert(k.clone());
                Some(k)
            };
            // UNIQUE-index probes (indexes.md §8), AFTER the primary-key duplicate check
            // (PG reports the PK first when both are violated — probed): per unique index
            // in catalog (name) order, a fully-non-NULL key tuple (its slot prefix) must
            // match no existing entry and no earlier row of this batch. Unmetered
            // validation, like the PK duplicate check (cost.md §3).
            for (u, def) in uniq_defs.iter().enumerate() {
                let Some(prefix) = index_prefix_key(columns, def, &row) else {
                    continue;
                };
                let istore = self.index_store(&def.name.to_ascii_lowercase());
                let stored = !istore
                    .range_entries(&unique_probe_bound(&prefix))?
                    .is_empty();
                if stored || !seen_prefixes[u].insert(prefix) {
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}",
                            def.name
                        ),
                    ));
                }
            }
            // Meter the row's disposition-plan compression attempts (value_compress, cost.md
            // §3). For a no-PK table the synthetic rowid is allocated in phase 2; only the key
            // LENGTH feeds the plan, so an 8-byte placeholder stands in deterministically.
            {
                let store = self.store(table);
                let placeholder = [0u8; 8];
                let kb: &[u8] = key.as_deref().unwrap_or(&placeholder);
                cunits += store.write_compress_units(kb, &row) as i64;
            }
            prepared.push((key, row));
        }

        // FOREIGN KEY existence (constraints.md §6.4) — after all candidate rows are prepared, so
        // the check sees the statement's batch END STATE (a later row may supply an earlier row's
        // parent key; a self-reference resolves within the batch — PG's end-of-statement
        // semantics). Unmetered validation, like the PK/UNIQUE probes, and before any write
        // (all-or-nothing). MATCH SIMPLE: a row with any NULL local column is exempt.
        let fks: Vec<ForeignKeyConstraint> = self
            .table(table)
            .map(|t| t.foreign_keys.clone())
            .unwrap_or_default();
        for fk in &fks {
            // The parent exists (validated at CREATE TABLE; DROP TABLE refuses to drop a
            // referenced table — §6.10), so a consistent catalog always finds it.
            let Some(parent) = self.table(&fk.ref_table) else {
                continue;
            };
            // Only a self-reference can satisfy against this statement's batch (a different parent
            // table is unchanged by this INSERT). Collect the parent keys the batch supplies.
            let batch: HashSet<Vec<u8>> = if fk.ref_table.eq_ignore_ascii_case(&relation) {
                prepared
                    .iter()
                    .filter_map(|(_, r)| {
                        fk_probe(fk, parent, r, &fk.ref_columns).map(|p| p.bytes().to_vec())
                    })
                    .collect()
            } else {
                HashSet::new()
            };
            for (_, row) in &prepared {
                let Some(probe) = fk_probe(fk, parent, row, &fk.columns) else {
                    continue; // a NULL local column → exempt (MATCH SIMPLE)
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                if !self.fk_probe_hits(&probe, &fk.ref_table)? {
                    return Err(EngineError::new(
                        SqlState::ForeignKeyViolation,
                        format!(
                            "insert or update on table {relation} violates foreign key constraint {}",
                            fk.name
                        ),
                    ));
                }
            }
        }

        // Charge + enforce the ceiling BEFORE phase 2 writes anything (all-or-nothing).
        meter.charge(COSTS.value_compress * cunits);
        meter.guard()?;

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the validated
        // rows — every check has passed, nothing is written yet, so subqueries in the list
        // read the pre-statement snapshot and a 54P01 here leaves the store untouched.
        let returned = match returning {
            Some(nodes) => {
                let prows: Vec<&Row> = prepared.iter().map(|(_, r)| r).collect();
                Some(self.project_returning(nodes, &prows, None, params, meter)?)
            }
            None => None,
        };

        // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
        // rowid is allocated here, in row order, so a failed validation pass burns none
        // (spec/fileformat/format.md, spec/design/grammar.md §12). Each stored row's
        // secondary-index entries are computed against its final key (the rowid included)
        // and written after the rows (indexes.md §4 — an index write cannot fail, so
        // all-or-nothing is unchanged).
        let store = self.store_mut(table);
        let mut index_inserts: Vec<Vec<Vec<u8>>> = vec![Vec::new(); indexes.len()];
        for (key, row) in prepared {
            let key = key.unwrap_or_else(|| encode_int(ScalarType::Int64, store.alloc_rowid()));
            for (k, def) in indexes.iter().enumerate() {
                index_inserts[k].extend(index_entry_keys(columns, def, &key, &row));
            }
            assert!(
                store.insert(key, row)?,
                "pre-validated INSERT key must be unique"
            );
        }
        for (k, def) in indexes.iter().enumerate() {
            let istore = self.index_store_mut(&def.name.to_ascii_lowercase());
            for ek in index_inserts[k].drain(..) {
                assert!(
                    istore.insert(ek, Vec::new())?,
                    "index entry keys are unique (storage-key suffix)"
                );
            }
        }
        Ok(returned)
    }

    /// Resolve a RETURNING item list against the target table's RETURNING scope
    /// (grammar.md §32; `Scope::returning` — the table at offset 0 plus the `old`/`new`
    /// qualifier-only pseudo-relations over the `[base | other]` projection row, with
    /// `base_is_old` true for DELETE): aggregates are 42803 (`Forbidden`), subqueries
    /// resolve (and may correlate against either row version), output names follow §8.
    /// Returns the projection nodes and names; the item types have no consumer. The INSERT
    /// path uses this (its target borrow ends early); UPDATE/DELETE resolve inline.
    fn resolve_returning(
        &self,
        table: &str,
        items: &SelectItems,
        base_is_old: bool,
        ptypes: &mut ParamTypes,
    ) -> Result<(Vec<RExpr>, Vec<String>, Vec<String>)> {
        let tdef = self.table(table).expect("INSERT target resolved above");
        let scope = Scope::returning(self, tdef, base_is_old);
        let (nodes, names, types) =
            resolve_projections(&scope, items, &mut AggCtx::Forbidden, ptypes)?;
        Ok((nodes, names, type_names(&types)))
    }

    /// Evaluate a resolved RETURNING projection over the affected rows (grammar.md §32,
    /// cost.md §3): per returned row, guard the ceiling, charge one `row_produced`, then
    /// evaluate each item — metered expression work, exactly a SELECT's projection (a
    /// correlated subquery re-runs here, its outer reference reading the row being
    /// returned). The evaluation row is the concatenation `[base | other]` the RETURNING
    /// scope resolved against: `others[i]` is the row's opposite version (UPDATE's old
    /// rows), `None` the all-NULL row (INSERT's old side, DELETE's new side). Callers run
    /// this after all validation and BEFORE any write.
    fn project_returning(
        &self,
        nodes: &[RExpr],
        rows: &[&Row],
        others: Option<&[&Row]>,
        params: &[Value],
        meter: &mut Meter,
    ) -> Result<Vec<Vec<Value>>> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer: &[],
            rng: &stmt_rng,
            ctes: CteCtx::empty(),
        };
        let mut out = Vec::with_capacity(rows.len());
        for (i, &row) in rows.iter().enumerate() {
            meter.guard()?;
            meter.charge(COSTS.row_produced);
            let mut combined = row.clone();
            match others {
                Some(olds) => combined.extend_from_slice(olds[i]),
                None => combined.resize(2 * row.len(), Value::Null),
            }
            let mut vals = Vec::with_capacity(nodes.len());
            for node in nodes {
                vals.push(node.eval(&combined, &env, meter)?);
            }
            out.push(vals);
        }
        Ok(out)
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
        // Capture the PK (index, type) now, by value, so the primary-key pushdown can be detected
        // after the `table` borrow ends (the mutate path takes `&mut self`). The index
        // definitions (and the columns their entry keys read) feed phase 2's maintenance
        // (indexes.md §4).
        let pk_info = table
            .primary_key_index()
            .map(|i| (i, table.columns[i].ty.scalar()));
        let ncols = table.columns.len();
        let indexes = table.indexes.clone();
        let tcolumns: Vec<Column> = if indexes.is_empty() {
            Vec::new()
        } else {
            table.columns.clone()
        };
        // DELETE is single-table; resolve its WHERE against a one-relation scope. The
        // RETURNING projection resolves after it (PostgreSQL's analysis order), against the
        // same scope (grammar.md §32).
        let scope = Scope::single(self, table);
        let mut ptypes = ParamTypes::default();
        let mut filter = match &del.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        // RETURNING resolves against its own scope: DELETE's base row IS the old row
        // (bare = `old.` = the deleted values; `new.` is the all-NULL side — grammar.md §32).
        let mut ret = match &del.returning {
            Some(items) => {
                let rscope = Scope::returning(self, table, true);
                let (nodes, names, types) =
                    resolve_projections(&rscope, items, &mut AggCtx::Forbidden, &mut ptypes)?;
                Some((nodes, names, type_names(&types)))
            }
            None => None,
        };
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated subqueries (in the WHERE or the RETURNING list) once
        // (their cost is added a single time — spec/design/grammar.md §26, cost.md §3); a
        // correlated one stays and re-runs per row via the per-row outer environment below.
        // The uncorrelated execution reads the pre-DELETE snapshot (we collect keys before
        // mutating), matching PostgreSQL.
        let mut meter = Meter::with_limit(self.max_cost);
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, CteCtx::empty(), &mut meter.accrued)?;
        }
        if let Some((nodes, _, _)) = &mut ret {
            for node in nodes {
                self.fold_uncorrelated_in_rexpr(node, &bound, CteCtx::empty(), &mut meter.accrued)?;
            }
        }

        // Collect matching (key, row) pairs before mutating (so the map is not modified
        // mid-scan; the rows feed phase 2's index-entry removal — indexed columns are
        // fixed-width and always resident). A WHERE arithmetic can trap (22003/22012), so
        // this is an explicit loop that propagates the error rather than a `.filter`
        // closure. Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13;
        // spec/design/cost.md §3).
        let mut matched: Vec<(Vec<u8>, Row)> = Vec::new();
        // A correlated subquery in the WHERE re-runs per row: the eval environment pushes the
        // current row, so `target.col` (an `OuterColumn`) reads it. `outer` starts empty (DELETE
        // is the top-level statement — no enclosing query).
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
            rng: &stmt_rng,
            ctes: CteCtx::empty(),
        };
        // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
        // scan"); an empty bound deletes nothing. The whole WHERE stays the residual filter below.
        // page_read per visited node (block, before the rows), then storage_row_read per scanned row.
        let pk_bound = match (&filter, pk_info) {
            (Some(f), Some((pk_idx, pk_ty))) => detect_pk_bound(f, pk_idx, pk_ty),
            _ => None,
        };
        // DELETE's touched set (cost.md §3): the filter's columns plus the RETURNING items'
        // OLD-side references — a returned old value is a logical read of the dropped row,
        // while a `new.col` is the constant NULL row and reads nothing. The RETURNING mask
        // spans the [base | other] projection row (2 x ncols); only the base (old) half maps
        // back to storage. A bare DELETE still charges no chain/decompress units at all.
        let mut mask = vec![false; ncols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut mask);
        }
        if let Some((nodes, _, _)) = &ret {
            let mut ret_mask = vec![false; 2 * ncols];
            for node in nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, m) in mask.iter_mut().enumerate() {
                *m |= ret_mask[i];
            }
        }
        let (entries, (overlap, slabs)) = match &pk_bound {
            // Top-level statement: no enclosing query, so the bound never has a correlated source.
            Some(bp) => match build_key_bound(bp, &bound, &[]) {
                Some(b) => {
                    let (entries, pages, slabs) =
                        self.store(&del.table).range_scan_with_units(&b, &mask)?;
                    (entries, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            None => {
                let (entries, pages, slabs) = self.store(&del.table).scan_with_units(&mask)?;
                (entries, (pages, slabs))
            }
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);
        let store = self.store(&del.table);
        for (k, mut row) in entries {
            meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
            meter.charge(COSTS.storage_row_read);
            // Materialize the filter's columns if the lazy load left them unfetched — exactly
            // the touched set the block above charged (large-values.md §14).
            store.resolve_columns(&mut row, &mask)?;
            let keep = match &filter {
                None => true,
                Some(f) => f.eval(&row, &env, &mut meter)?.is_true(),
            };
            if keep {
                matched.push((k, row));
            }
        }

        // FOREIGN KEY parent-side (constraints.md §6.5): a DELETE must not strand a child. For
        // each inbound FK, every deleted row's referenced tuple disappears (the referenced columns
        // are unique, so each is unique to its row); if a child still references it → 23503.
        // Unmetered, before phase 2 (all-or-nothing). For a self-reference the child IS this table,
        // whose end state excludes the rows being deleted.
        let referencers = self.fk_referencers(&del.table);
        if !referencers.is_empty() {
            let parent = self
                .table(&del.table)
                .expect("delete target exists")
                .clone();
            let deleted_keys: HashSet<Vec<u8>> = matched.iter().map(|(k, _)| k.clone()).collect();
            let empty: HashSet<Vec<u8>> = HashSet::new();
            for (child_table, fk) in &referencers {
                let exclude = if child_table.eq_ignore_ascii_case(&del.table) {
                    &deleted_keys
                } else {
                    &empty
                };
                for (_, row) in &matched {
                    let Some(probe) = fk_probe(fk, &parent, row, &fk.ref_columns) else {
                        continue; // a NULL referenced value cannot be referenced (MATCH SIMPLE)
                    };
                    if self.fk_child_references(child_table, fk, &parent, probe.bytes(), exclude)? {
                        return Err(EngineError::new(
                            SqlState::ForeignKeyViolation,
                            format!(
                                "update or delete on table {} violates foreign key constraint {} on table {}",
                                parent.name, fk.name, child_table
                            ),
                        ));
                    }
                }
            }
        }

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched
        // rows' OLD values before anything is removed — subqueries in the list read the
        // pre-statement snapshot, and a 54P01 here deletes nothing (all-or-nothing).
        let returned = match &ret {
            Some((nodes, _, _)) => {
                let prows: Vec<&Row> = matched.iter().map(|(_, r)| r).collect();
                Some(self.project_returning(nodes, &prows, None, &bound, &mut meter)?)
            }
            None => None,
        };

        // Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
        // unmetered write work; an index removal cannot fail).
        let store = self.store_mut(&del.table);
        for (k, _) in &matched {
            store.remove(k)?;
        }
        for def in &indexes {
            let istore = self.index_store_mut(&def.name.to_ascii_lowercase());
            for (k, row) in &matched {
                for ek in index_entry_keys(&tcolumns, def, k, row) {
                    istore.remove(&ek)?;
                }
            }
        }
        Ok(match (ret, returned) {
            (Some((_, names, types)), Some(rows)) => Outcome::Query {
                column_names: names,
                column_types: types,
                rows,
                cost: meter.accrued,
            },
            _ => Outcome::Statement {
                cost: meter.accrued,
                rows_affected: Some(matched.len() as i64),
            },
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

        // Resolve assignments up front (fail fast, deterministic). The 0A000 guard covers
        // EVERY key member — for a composite PRIMARY KEY, assigning any member would change
        // the storage key (constraints.md §3).
        let pk_members = table.pk_indices();
        // Capture the PK (index, type) by value for the primary-key pushdown (detected after the
        // `table` borrow ends, since the mutate path takes `&mut self`). Pushdown recognizes
        // single-column keys only (`primary_key_index`); a composite-PK table full-scans.
        let pk_info = table
            .primary_key_index()
            .map(|i| (i, table.columns[i].ty.scalar()));
        let ncols = table.columns.len();
        // The index definitions (and the columns their entry keys read) feed phase 2's
        // maintenance (indexes.md §4): an entry moves only when its key actually changed.
        let indexes = table.indexes.clone();
        let tcolumns: Vec<Column> = if indexes.is_empty() {
            Vec::new()
        } else {
            table.columns.clone()
        };
        let mut ptypes = ParamTypes::default();
        let mut plans: Vec<AssignPlan> = Vec::with_capacity(upd.assignments.len());
        for a in &upd.assignments {
            let idx = col_idx(table, &a.column)?;
            if pk_members.contains(&idx) {
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
            // Updating a composite-typed column lands in a later slice (the storable + INSERT/SELECT
            // round-trip is S3 — spec/design/composite.md §12); reject it for now (0A000).
            let Type::Scalar(target_scalar) = &col.ty else {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "updating composite column {} is not supported yet",
                        a.column
                    ),
                ));
            };
            let target_scalar = *target_scalar;
            // The RHS is a general expression evaluated against the *old* row; a literal
            // operand adapts to the target column's type. The result must be assignable to
            // the column's family (integer/decimal/text or NULL; never boolean; decimal→int
            // is explicit-CAST only) — spec/design/decimal.md §6.
            let (source, ty) = resolve(
                &scope,
                &a.value,
                Some(target_scalar),
                &mut AggCtx::Forbidden,
                &mut ptypes,
            )?;
            require_assignable(&ty, target_scalar, &a.column)?;
            plans.push(AssignPlan {
                idx,
                name: col.name.clone(),
                target: target_scalar,
                decimal: col.decimal,
                not_null: col.not_null,
                source,
            });
        }

        let mut filter = match &upd.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        // The RETURNING projection resolves last (PostgreSQL's analysis order), against its
        // own scope: UPDATE's base row is the NEW row (bare = `new.` = post-assignment), and
        // `old.` reads the pre-update half of [base | other] (grammar.md §32).
        let mut ret = match &upd.returning {
            Some(items) => {
                let rscope = Scope::returning(self, table, false);
                let (nodes, names, types) =
                    resolve_projections(&rscope, items, &mut AggCtx::Forbidden, &mut ptypes)?;
                Some((nodes, names, type_names(&types)))
            }
            None => None,
        };
        // The CHECK constraints, resolved once per statement in evaluation (name) order;
        // phase 1 evaluates them on each post-assignment row (constraints.md §4.4).
        let checks = self.resolve_checks(table)?;
        let relation = table.name.clone();
        // All assignment RHSs + the WHERE + the RETURNING are resolved: finalize + bind
        // before any scan.
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated subqueries (in any assignment RHS or the WHERE) once — their
        // cost is added a single time (grammar.md §26, cost.md §3); a correlated one stays and
        // re-runs per row via the outer environment. The uncorrelated execution reads the
        // pre-UPDATE snapshot (phase 1 only reads; phase 2 writes), matching PostgreSQL.
        let mut meter = Meter::with_limit(self.max_cost);
        for plan in &mut plans {
            self.fold_uncorrelated_in_rexpr(
                &mut plan.source,
                &bound,
                CteCtx::empty(),
                &mut meter.accrued,
            )?;
        }
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, CteCtx::empty(), &mut meter.accrued)?;
        }
        if let Some((nodes, _, _)) = &mut ret {
            for node in nodes {
                self.fold_uncorrelated_in_rexpr(node, &bound, CteCtx::empty(), &mut meter.accrued)?;
            }
        }

        // Phase 1: build + validate every matching row's new values; no writes yet. Each
        // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes
        // do not — they evaluate nothing; spec/design/cost.md §3). Each entry is
        // (key, new row, OLD row) — the old row feeds the index maintenance.
        let mut updates: Vec<(Vec<u8>, Row, Row)> = Vec::new();
        // A correlated subquery (in an RHS or the WHERE) re-runs per row: the eval environment
        // pushes the current (old) row, so `target.col` (an `OuterColumn`) reads it. `outer`
        // starts empty (UPDATE is the top-level statement — no enclosing query).
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
            rng: &stmt_rng,
            ctes: CteCtx::empty(),
        };
        // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
        // scan"); an empty bound updates nothing. The whole WHERE stays the residual filter below.
        // page_read per visited node (block, before the rows), then storage_row_read per scanned row.
        let pk_bound = match (&filter, pk_info) {
            (Some(f), Some((pk_i, pk_ty))) => detect_pk_bound(f, pk_i, pk_ty),
            _ => None,
        };
        // UPDATE's touched set (cost.md §3): the filter's columns, every assignment SOURCE's,
        // and the RETURNING items' — the NEW side minus the assigned columns (an assigned
        // column's returned value is the freshly computed one, not a storage read), plus the
        // OLD side unconditionally (`old.col` is always a storage read, assigned or not; the
        // RETURNING mask spans the [base | other] projection row, new at 0, old at ncols).
        // The rewrite re-stores an untouched spilled value without logically re-reading it
        // (large-values.md §14).
        let mut mask = vec![false; ncols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut mask);
        }
        for plan in &plans {
            collect_touched(&plan.source, 0, &mut mask);
        }
        if let Some((nodes, _, _)) = &ret {
            let mut ret_mask = vec![false; 2 * ncols];
            for node in nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, m) in mask.iter_mut().enumerate() {
                *m |= ret_mask[i] && !plans.iter().any(|p| p.idx == i); // new side
                *m |= ret_mask[ncols + i]; // old side — always a storage read
            }
        }
        let (entries, (overlap, slabs)) = match &pk_bound {
            // Top-level statement: no enclosing query, so the bound never has a correlated source.
            Some(bp) => match build_key_bound(bp, &bound, &[]) {
                Some(b) => {
                    let (entries, pages, slabs) =
                        self.store(&upd.table).range_scan_with_units(&b, &mask)?;
                    (entries, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            None => {
                let (entries, pages, slabs) = self.store(&upd.table).scan_with_units(&mask)?;
                (entries, (pages, slabs))
            }
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);
        let store = self.store(&upd.table);
        for (key, mut row) in entries {
            meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
            meter.charge(COSTS.storage_row_read);
            // Materialize the filter's + assignment sources' columns if the lazy load left them
            // unfetched — exactly the touched set the block above charged (large-values.md §14).
            store.resolve_columns(&mut row, &mask)?;
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(&row, &env, &mut meter)?.is_true(),
            };
            if !matched {
                continue;
            }
            let mut new_row = row.clone();
            for plan in &plans {
                let raw = plan.source.eval(&row, &env, &mut meter)?;
                new_row[plan.idx] = plan.check(raw)?;
            }
            // The rewritten row is stored fully resident: resolve any still-unfetched (untouched)
            // columns so its weight/disposition re-plan exactly as an eager writer's would —
            // unmetered, part of the rewrite like commit work (large-values.md §14).
            store.resolve_all(&mut new_row)?;
            // CHECK constraints, in name order, on the post-assignment row — after the
            // assignments coerced (22003/23502 in `plan.check` above), on the fully-resident
            // row (constraints.md §4.4). Every check evaluates (not only those mentioning
            // assigned columns); TRUE and NULL pass, the first FALSE aborts the statement
            // (phase 1 — nothing has been written).
            for (name, rexpr) in &checks {
                if matches!(rexpr.eval(&new_row, &env, &mut meter)?, Value::Bool(false)) {
                    return Err(EngineError::new(
                        SqlState::CheckViolation,
                        format!("new row for relation {relation} violates check constraint {name}"),
                    ));
                }
            }
            updates.push((key, new_row, row));
        }

        // UNIQUE validation against the statement's END STATE (indexes.md §8 — a
        // documented PG divergence: PG checks per-row in heap order, so a transient
        // collision like `SET v = v + 1` fails there and succeeds here). Per unique index
        // in catalog (name) order, over the rewritten rows in scan (storage-key) order:
        // the new prefixes must not collide with each other (in-batch), nor with an
        // existing entry whose suffix is NOT a rewritten row's key (a rewritten row's old
        // entry is being replaced, so it cannot conflict). Unmetered validation, phase 1.
        if indexes.iter().any(|d| d.unique) && !updates.is_empty() {
            let rewritten: HashSet<&[u8]> = updates.iter().map(|(k, _, _)| k.as_slice()).collect();
            for def in indexes.iter().filter(|d| d.unique) {
                let istore = self.index_store(&def.name.to_ascii_lowercase());
                let mut batch: HashSet<Vec<u8>> = HashSet::new();
                for (_, new_row, _) in &updates {
                    let Some(prefix) = index_prefix_key(&tcolumns, def, new_row) else {
                        continue;
                    };
                    let conflict = !batch.insert(prefix.clone())
                        || istore
                            .range_entries(&unique_probe_bound(&prefix))?
                            .iter()
                            .any(|(ekey, _)| !rewritten.contains(&ekey[prefix.len()..]));
                    if conflict {
                        return Err(EngineError::new(
                            SqlState::UniqueViolation,
                            format!(
                                "duplicate key value violates unique constraint: {}",
                                def.name
                            ),
                        ));
                    }
                }
            }
        }

        // FOREIGN KEY child-side (constraints.md §6.4): re-validate an FK only when the statement
        // assigns one of its local columns (an unchanged value stays valid). Each updated NEW row
        // must reference an existing parent key — committed parent state, plus (for a
        // self-reference) the updated rows' new referenced values, so a row may reference a value
        // another updated row now supplies. Unmetered, phase 1, before any write.
        let assigned: HashSet<usize> = plans.iter().map(|p| p.idx).collect();
        let fks: Vec<ForeignKeyConstraint> = self
            .table(&upd.table)
            .map(|t| t.foreign_keys.clone())
            .unwrap_or_default();
        for fk in &fks {
            if !fk.columns.iter().any(|c| assigned.contains(c)) {
                continue; // this FK's local columns were not assigned
            }
            let Some(parent) = self.table(&fk.ref_table) else {
                continue;
            };
            let batch: HashSet<Vec<u8>> = if fk.ref_table.eq_ignore_ascii_case(&relation) {
                updates
                    .iter()
                    .filter_map(|(_, new_row, _)| {
                        fk_probe(fk, parent, new_row, &fk.ref_columns).map(|p| p.bytes().to_vec())
                    })
                    .collect()
            } else {
                HashSet::new()
            };
            for (_, new_row, _) in &updates {
                let Some(probe) = fk_probe(fk, parent, new_row, &fk.columns) else {
                    continue; // a NULL local column → exempt (MATCH SIMPLE)
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                if !self.fk_probe_hits(&probe, &fk.ref_table)? {
                    return Err(EngineError::new(
                        SqlState::ForeignKeyViolation,
                        format!(
                            "insert or update on table {relation} violates foreign key constraint {}",
                            fk.name
                        ),
                    ));
                }
            }
        }

        // FOREIGN KEY parent-side (constraints.md §6.5): an UPDATE of a referenced row must not
        // strand a child. A referenced PRIMARY KEY column cannot change (PK assignment is 0A000),
        // so only a referenced UNIQUE column is at risk. For each inbound FK, a referenced tuple
        // DISAPPEARS when an updated row's old value is absent from the statement's new end state
        // (`old − new` over the updated rows); if a child still references a disappearing tuple →
        // 23503. Unmetered, phase 1. A self-reference's child IS this table, whose end state
        // excludes the rows being updated (their new values are validated child-side above).
        let referencers = self.fk_referencers(&upd.table);
        if !referencers.is_empty() {
            let parent = self
                .table(&upd.table)
                .expect("update target exists")
                .clone();
            let updated_keys: HashSet<Vec<u8>> =
                updates.iter().map(|(k, _, _)| k.clone()).collect();
            let empty: HashSet<Vec<u8>> = HashSet::new();
            for (child_table, fk) in &referencers {
                // The referenced tuples the updated rows now supply (so a swap re-supplies one).
                let new_present: HashSet<Vec<u8>> = updates
                    .iter()
                    .filter_map(|(_, new_row, _)| {
                        fk_probe(fk, &parent, new_row, &fk.ref_columns).map(|p| p.bytes().to_vec())
                    })
                    .collect();
                let exclude = if child_table.eq_ignore_ascii_case(&upd.table) {
                    &updated_keys
                } else {
                    &empty
                };
                for (_, new_row, old_row) in &updates {
                    let Some(old_probe) = fk_probe(fk, &parent, old_row, &fk.ref_columns) else {
                        continue; // a NULL old referenced value was referenced by nothing
                    };
                    // Unchanged tuples (incl. a NULL→ already skipped) do not disappear.
                    if let Some(new_probe) = fk_probe(fk, &parent, new_row, &fk.ref_columns) {
                        if new_probe.bytes() == old_probe.bytes() {
                            continue;
                        }
                    }
                    // Re-supplied by another updated row (e.g. a value swap) → not disappearing.
                    if new_present.contains(old_probe.bytes()) {
                        continue;
                    }
                    if self.fk_child_references(
                        child_table,
                        fk,
                        &parent,
                        old_probe.bytes(),
                        exclude,
                    )? {
                        return Err(EngineError::new(
                            SqlState::ForeignKeyViolation,
                            format!(
                                "update or delete on table {} violates foreign key constraint {} on table {}",
                                parent.name, fk.name, child_table
                            ),
                        ));
                    }
                }
            }
        }

        // Each rewritten row's disposition plan may attempt compression (a record over
        // RECORD_MAX) — meter the attempts (value_compress, cost.md §3) and enforce the
        // ceiling BEFORE phase 2 writes anything, preserving all-or-nothing.
        let store = self.store(&upd.table);
        let mut cunits: i64 = 0;
        for (key, row, _) in &updates {
            cunits += store.write_compress_units(key, row) as i64;
        }
        meter.charge(COSTS.value_compress * cunits);
        meter.guard()?;

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched
        // rows' NEW (post-assignment, fully resident) values — all validation has passed,
        // nothing is written yet, so subqueries in the list read the pre-statement snapshot
        // and a 54P01 here writes nothing (all-or-nothing).
        let returned = match &ret {
            Some((nodes, _, _)) => {
                let prows: Vec<&Row> = updates.iter().map(|(_, new_row, _)| new_row).collect();
                let olds: Vec<&Row> = updates.iter().map(|(_, _, old_row)| old_row).collect();
                Some(self.project_returning(nodes, &prows, Some(&olds), &bound, &mut meter)?)
            }
            None => None,
        };

        // Index maintenance (indexes.md §4): an entry moves only when its key CHANGED —
        // equal old/new keys leave the index tree untouched (part of the contract: it keeps
        // the copy-on-write dirty set, and so the commit's written pages, byte-identical
        // across cores). The storage key cannot change (PK assignment is rejected), so the
        // suffix is stable. Computed before the rewrite consumes the rows.
        let mut index_moves: Vec<Vec<(Vec<Vec<u8>>, Vec<Vec<u8>>)>> =
            vec![Vec::new(); indexes.len()];
        for (key, new_row, old_row) in &updates {
            for (k, def) in indexes.iter().enumerate() {
                // The row's old and new entry SETS (one entry for an ordered index, one per term
                // for GIN — gin.md §5). Remove old−new, insert new−old: a shared entry (an ordered
                // key that did not change, or a GIN term present in both) is left untouched,
                // keeping the copy-on-write dirty set byte-identical across cores.
                let old_eks = index_entry_keys(&tcolumns, def, key, old_row);
                let new_eks = index_entry_keys(&tcolumns, def, key, new_row);
                let removals: Vec<Vec<u8>> = old_eks
                    .iter()
                    .filter(|e| !new_eks.contains(*e))
                    .cloned()
                    .collect();
                let insertions: Vec<Vec<u8>> = new_eks
                    .iter()
                    .filter(|e| !old_eks.contains(*e))
                    .cloned()
                    .collect();
                if !removals.is_empty() || !insertions.is_empty() {
                    index_moves[k].push((removals, insertions));
                }
            }
        }

        // Phase 2: apply (keys unchanged — a PK column can't be assigned), then move the
        // changed index entries (unmetered write work; cannot fail).
        let updated = updates.len() as i64;
        let store = self.store_mut(&upd.table);
        for (key, row, _) in updates {
            store.replace(&key, row)?;
        }
        for (k, def) in indexes.iter().enumerate() {
            let istore = self.index_store_mut(&def.name.to_ascii_lowercase());
            for (removals, insertions) in index_moves[k].drain(..) {
                for old_ek in removals {
                    istore.remove(&old_ek)?;
                }
                for new_ek in insertions {
                    assert!(
                        istore.insert(new_ek, Vec::new())?,
                        "index entry keys are unique (storage-key suffix)"
                    );
                }
            }
        }
        Ok(match (ret, returned) {
            (Some((_, names, types)), Some(rows)) => Outcome::Query {
                column_names: names,
                column_types: types,
                rows,
                cost: meter.accrued,
            },
            _ => Outcome::Statement {
                cost: meter.accrued,
                rows_affected: Some(updated),
            },
        })
    }

    /// Run a SELECT as a top-level statement: `run_select`, then wrap as a query Outcome
    /// (the projection types are internal — only `INSERT ... SELECT` consumes them).
    fn execute_select(&mut self, sel: Select, params: &[Value]) -> Result<Outcome> {
        let r = self.run_select(sel, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            column_types: type_names(&r.column_types),
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
            column_types: type_names(&r.column_types),
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Execute a `WITH` query (spec/design/cte.md) — the host-API entry point; `run_with` does the
    /// CTE orchestration.
    fn execute_with(&mut self, wq: WithQuery, params: &[Value]) -> Result<Outcome> {
        let r = self.run_with(wq, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            column_types: type_names(&r.column_types),
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
        let mut plan = self.plan_query(&qe, None, &[], &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?)?;
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_plan(&mut plan, &bound, CteCtx::empty(), &mut subquery_cost)?;
        let mut r = self.exec_query_plan(&plan, &[], &bound, CteCtx::empty())?;
        r.cost += subquery_cost;
        Ok(r)
    }

    /// Run a `WITH` query (spec/design/cte.md). The CTE orchestrator (the critique's `plan_with`):
    /// (1) PLAN each CTE body in order against the prefix of earlier bindings (parent = None — a
    /// body is an independent query, NOT correlated to a reference site), deriving each binding's
    /// synthetic relation; (2) plan the main body with all bindings visible, threading the one
    /// `ParamTypes` so `$N` infers statement-wide; (3) decide each CTE's mode from its reference
    /// count + `[NOT] MATERIALIZED` hint; (4) MATERIALIZE each referenced materialized CTE once, in
    /// list order, accruing its cost (a later body sees the earlier buffers); (5) fold + EXECUTE the
    /// main body with the CTE context. Cost composes like set operations — a sum of the parts.
    fn run_with(&self, wq: WithQuery, params: &[Value]) -> Result<SelectResult> {
        let mut ptypes = ParamTypes::default();
        // (1) Plan each CTE body against the already-built prefix; build its synthetic relation.
        let mut bindings: Vec<CteBinding> = Vec::with_capacity(wq.ctes.len());
        for cte in &wq.ctes {
            let lname = cte.name.to_ascii_lowercase();
            if bindings.iter().any(|b| b.name == lname) {
                return Err(EngineError::new(
                    SqlState::DuplicateAlias,
                    format!("WITH query name {lname} specified more than once"),
                ));
            }
            let plan = self.plan_query(&cte.query, None, &bindings, &mut ptypes)?;
            let table = cte_synthetic_table(&lname, &plan, cte.columns.as_deref())?;
            bindings.push(CteBinding {
                name: lname,
                table,
                plan,
                hint: cte.materialized,
                refs: std::cell::Cell::new(0),
            });
        }
        // (2) Plan the main body with all bindings visible.
        let mut plan = self.plan_query(&wq.body, None, &bindings, &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // (3) Per-CTE evaluation mode: MATERIALIZED hint or >=2 references -> Materialize, else
        //     Inline (cost.md §3). An unreferenced CTE is planned (errors surfaced) but not run.
        let modes: Vec<CteMode> = bindings
            .iter()
            .map(|b| match b.hint {
                Some(true) => CteMode::Materialize,
                Some(false) => CteMode::Inline,
                None if b.refs.get() >= 2 => CteMode::Materialize,
                None => CteMode::Inline,
            })
            .collect();
        let plans: Vec<QueryPlan> = bindings.into_iter().map(|b| b.plan).collect();

        // (4) Materialize each referenced materialized CTE once, in list order, accruing cost. A
        //     later body's inline/materialized reference to an earlier CTE sees the prefix context.
        let mut total_cost: i64 = 0;
        let mut buffers: Vec<Vec<Row>> = Vec::with_capacity(plans.len());
        for (i, p) in plans.iter().enumerate() {
            let buf = if modes[i] == CteMode::Materialize {
                let ctx = CteCtx {
                    modes: &modes[..i],
                    plans: &plans[..i],
                    buffers: &buffers,
                };
                let r = self.exec_query_plan(p, &[], &bound, ctx)?;
                total_cost += r.cost;
                r.rows
            } else {
                Vec::new()
            };
            buffers.push(buf);
        }

        // (5) Fold + execute the main body against the full CTE context.
        let ctx = CteCtx {
            modes: &modes,
            plans: &plans,
            buffers: &buffers,
        };
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_plan(&mut plan, &bound, ctx, &mut subquery_cost)?;
        let mut r = self.exec_query_plan(&plan, &[], &bound, ctx)?;
        r.cost += subquery_cost + total_cost;
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
    fn plan_query<'a>(
        &'a self,
        qe: &QueryExpr,
        parent: Option<&Scope<'a>>,
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        match qe {
            QueryExpr::Select(sel) => Ok(QueryPlan::Select(
                self.plan_select(sel, parent, ctes, ptypes)?,
            )),
            QueryExpr::SetOp(so) => Ok(QueryPlan::SetOp(Box::new(
                self.plan_set_op(so, parent, ctes, ptypes)?,
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
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        match plan {
            QueryPlan::Select(sp) => self.exec_select_plan(sp, outer, params, ctes),
            QueryPlan::SetOp(sop) => self.exec_set_op_plan(sop, outer, params, ctes),
            QueryPlan::Values(vp) => self.exec_values_plan(vp, outer, params, ctes),
        }
    }

    /// Plan a set operation (spec/design/grammar.md §25): plan both operands with the same
    /// parent scope, check arity + unify column types up front (so the 42601/42804 fire even
    /// over empty operands), and resolve the trailing ORDER BY by output column name.
    fn plan_set_op<'a>(
        &'a self,
        so: &SetOp,
        parent: Option<&Scope<'a>>,
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<SetOpPlan> {
        let lhs = self.plan_query(&so.lhs, parent, ctes, ptypes)?;
        let rhs = self.plan_query(&so.rhs, parent, ctes, ptypes)?;

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
            .map(|(l, r)| unify_setop_column(l, r, so.op))
            .collect::<Result<_>>()?;
        let column_names = match &lhs {
            QueryPlan::Select(s) => s.column_names.clone(),
            QueryPlan::SetOp(s) => s.column_names.clone(),
            QueryPlan::Values(v) => v.column_names.clone(),
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

    /// Resolve a VALUES-body relation into a `ValuesPlan` (spec/design/grammar.md §42) — the body
    /// of a `FROM (VALUES …)` derived table. Each value resolves as a CONSTANT against an EMPTY
    /// scope with `parent = None`: the body is non-`LATERAL`, so a column reference is unresolved
    /// (42703/42P01) and an aggregate is 42803; it still sees the statement's CTE bindings (an
    /// uncorrelated subquery inside a value resolves like anywhere). Every row must have the same
    /// arity (42601); the columns' types unify across rows like a set operation (42804 on a
    /// mismatch). A bind parameter is then noted at its column's unified type (so `VALUES (1),($1)`
    /// types `$1` as `int`); a column with no concrete type — all NULL/param — leaves its `$N`
    /// untyped, surfacing 42P18 at `finalize` (jed's no-cross-context inference posture, §26).
    fn plan_values<'a>(
        &'a self,
        rows: &[Vec<Expr>],
        parent: Option<&Scope<'a>>,
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<ValuesPlan> {
        // The parser guarantees at least one row, each with at least one value.
        let arity = rows[0].len();
        // A constant scope: no local relations. With `parent = None` (the usual case) any column
        // reference is unresolved (the non-`LATERAL` rule, §42); with a `parent` (a `LATERAL`
        // VALUES body, §44) a column reference correlates to the earlier FROM relations instead.
        // CTE bindings stay visible and subqueries are allowed (an uncorrelated one folds early).
        let scope = Scope {
            rels: Vec::new(),
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
        };
        let mut resolved_rows: Vec<Vec<RExpr>> = Vec::with_capacity(rows.len());
        let mut col_types: Vec<ResolvedType> = Vec::with_capacity(arity);
        // Per column: the 0-based bind-parameter slots appearing in it, typed in a second pass from
        // the unified column type (a $N takes its column's type, like a set-operation operand).
        let mut col_params: Vec<Vec<usize>> = vec![Vec::new(); arity];
        for (ri, row) in rows.iter().enumerate() {
            if row.len() != arity {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "VALUES lists must all be the same length",
                ));
            }
            let mut resolved_row = Vec::with_capacity(arity);
            for (ci, val) in row.iter().enumerate() {
                // Aggregates are not allowed in a VALUES list (a stray one is 42803).
                let mut agg = AggCtx::Forbidden;
                let (node, ty) = resolve(&scope, val, None, &mut agg, ptypes)?;
                if let RExpr::Param(idx0) = &node {
                    col_params[ci].push(*idx0);
                }
                if ri == 0 {
                    col_types.push(ty);
                } else {
                    col_types[ci] = unify_values_column(&col_types[ci], &ty)?;
                }
                resolved_row.push(node);
            }
            resolved_rows.push(resolved_row);
        }
        // Second pass: note each column's bind parameters at the unified column type. A column with
        // no scalar type (all NULL/param) passes `None` — the parameter stays untyped (42P18).
        for (ci, params_here) in col_params.iter().enumerate() {
            let hint = scalar_for_param_hint(&col_types[ci]);
            for &idx0 in params_here {
                ptypes.note(idx0, hint)?;
            }
        }
        // PostgreSQL names a VALUES relation's columns column1, column2, … ; the derived table's
        // optional column-rename list overrides them at the synthetic relation (cte_synthetic_table).
        let column_names = (1..=arity).map(|i| format!("column{i}")).collect();
        Ok(ValuesPlan {
            rows: resolved_rows,
            column_types: col_types,
            column_names,
        })
    }

    /// Execute a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate each row's
    /// values as constants over an EMPTY environment (no local row, no outer row — non-`LATERAL`),
    /// coerce each to the unified column type (the only runtime change is int → decimal, the
    /// set-operation rule), and emit the rows. Charges `row_produced` per row plus each value's
    /// `operator_eval` (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded
    /// into the caller's meter via `exec_query_plan`.
    fn exec_values_plan(
        &self,
        plan: &ValuesPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng: &stmt_rng,
            ctes,
        };
        let mut meter = Meter::with_limit(self.max_cost);
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(plan.rows.len());
        for row in &plan.rows {
            meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
            meter.charge(COSTS.row_produced);
            let mut out = Vec::with_capacity(plan.column_types.len());
            for (ci, e) in row.iter().enumerate() {
                let v = e.eval(&[], &env, &mut meter)?;
                // Int → decimal where the column unified to decimal (the set-operation rule); every
                // other unified type is a value no-op (int-width promotion is free — all ints are i64).
                let v = match (&plan.column_types[ci], &v) {
                    (ResolvedType::Decimal, Value::Int(n)) => Value::Decimal(Decimal::from_i64(*n)),
                    _ => v,
                };
                out.push(v);
            }
            rows.push(out);
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows,
            cost: meter.accrued,
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
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let left = self.exec_query_plan(&plan.lhs, outer, params, ctes)?;
        let right = self.exec_query_plan(&plan.rhs, outer, params, ctes)?;

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
    fn plan_select<'a>(
        &'a self,
        sel: &Select,
        parent: Option<&Scope<'a>>,
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<SelectPlan> {
        // Build the FROM scope (spec/design/grammar.md §15/§44): resolve each table reference (42P01
        // if unknown), compute its flat column offset in FROM order, reject a duplicate label (42712),
        // and — for a LATERAL item — resolve its body / SRF args against the PREFIX of relations to
        // its left (the dependent-join scope, §44). A FROM-less SELECT (`sel.from` = None) builds an
        // EMPTY scope: bare columns fall through to `parent` (correlation) or 42703 at top level
        // (§34). The scope links to `parent` (for correlation) and the catalog; `allow_subquery` is
        // true (UPDATE/DELETE pass a `Scope::single` with it false).
        //   An SRF / derived table has no catalog table — its relation borrows a SYNTHETIC `Table`
        // that must outlive the scope, so the synthetic tables live in a local `Vec<Box<Table>>` and
        // `rels` borrows into it. Because a LATERAL item resolves against the EARLIER synthetic tables
        // WHILE later ones are still being pushed, the build runs in FROM order, recording each
        // finalized relation in `finalized` (a synthetic table by INDEX, holding no borrow that would
        // block a later push); the persistent `rels`/scope is assembled from `finalized` afterwards.
        let from_items: Vec<&TableRef> = sel
            .from
            .iter()
            .chain(sel.joins.iter().map(|j| &j.table))
            .collect();
        let mut synthetic: Vec<Box<Table>> = Vec::new();
        // Per FROM item: `None` = a base table; `Some((synthetic_index, srf_args, kind))` = an SRF.
        let mut srf_meta: Vec<Option<(usize, Vec<RExpr>, SrfKind)>> =
            Vec::with_capacity(from_items.len());
        // Per FROM item: the planned body of a DERIVED TABLE (grammar.md §42), else `None`.
        let mut derived_plans: Vec<Option<QueryPlan>> = Vec::with_capacity(from_items.len());
        // Per FROM item: the index into `synthetic` of a derived table's relation, else `None`.
        let mut derived_meta: Vec<Option<usize>> = Vec::with_capacity(from_items.len());
        // Per FROM item: true when it is a CORRELATED lateral relation (§44) — its body / SRF args
        // reference an earlier sibling (or an enclosing query), so the executor re-materializes it per
        // combined left-hand row. A non-correlated item (or the first item) is materialized once.
        let mut lateral_flags: Vec<bool> = Vec::with_capacity(from_items.len());
        // The relations finalized so far (label + flat offset + table source), used to build the
        // prefix `parent` scope a LATERAL item resolves against, then to assemble `rels`.
        let mut finalized: Vec<FinalRel> = Vec::with_capacity(from_items.len());
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        for (i, tref) in from_items.iter().enumerate() {
            let is_derived = tref.subquery.is_some() || tref.values.is_some();
            // A FROM item is lateral-ELIGIBLE when it can see earlier siblings: a derived table /
            // VALUES body explicitly marked `LATERAL`, or ANY table function (implicitly lateral —
            // §44). The first item (i == 0) has no earlier sibling, so it is never lateral; an SRF
            // there resolves against `parent` (the enclosing query) exactly as before.
            let lateral_eligible = i > 0 && ((is_derived && tref.lateral) || tref.args.is_some());
            let src: RelSrc;
            if is_derived {
                // Plan the body. LATERAL → `parent` is the prefix scope (earlier siblings chained to
                // the enclosing query, so a sibling/outer column correlates); otherwise the body is an
                // INDEPENDENT query (`parent = None`, §42). A LATERAL VALUES body resolves its values
                // against the prefix too (a column ref then correlates instead of 42703).
                let plan = if lateral_eligible {
                    let prefix = build_prefix_scope(&finalized, &synthetic, parent, self, ctes);
                    match (&tref.subquery, &tref.values) {
                        (Some(body), _) => self.plan_query(body, Some(&prefix), ctes, ptypes)?,
                        (None, Some(rows)) => QueryPlan::Values(self.plan_values(
                            rows,
                            Some(&prefix),
                            ctes,
                            ptypes,
                        )?),
                        _ => unreachable!(),
                    }
                } else {
                    match (&tref.subquery, &tref.values) {
                        (Some(body), _) => self.plan_query(body, None, ctes, ptypes)?,
                        (None, Some(rows)) => {
                            QueryPlan::Values(self.plan_values(rows, None, ctes, ptypes)?)
                        }
                        _ => unreachable!(),
                    }
                };
                lateral_flags.push(lateral_eligible && query_plan_references_outer(&plan, 0));
                let label = tref.alias.clone().unwrap_or_default().to_ascii_lowercase();
                let table = cte_synthetic_table(&label, &plan, tref.column_aliases.as_deref())?;
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(None);
                derived_meta.push(Some(si));
                derived_plans.push(Some(plan));
                src = RelSrc::Synthetic(si);
            } else if let Some(args) = &tref.args {
                // A table function (SRF) — implicitly lateral. At i>0 its args resolve against the
                // prefix scope (a sibling column then correlates); at i==0 against `parent` (the
                // enclosing query / params), unchanged (functions.md §10).
                let (table, rargs, kind) = if lateral_eligible {
                    let prefix = build_prefix_scope(&finalized, &synthetic, parent, self, ctes);
                    self.resolve_srf(
                        &tref.name,
                        args,
                        tref.alias.as_deref(),
                        Some(&prefix),
                        ctes,
                        ptypes,
                    )?
                } else {
                    self.resolve_srf(
                        &tref.name,
                        args,
                        tref.alias.as_deref(),
                        parent,
                        ctes,
                        ptypes,
                    )?
                };
                lateral_flags
                    .push(lateral_eligible && rargs.iter().any(|a| rexpr_references_outer(a, 0)));
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(Some((si, rargs, kind)));
                derived_meta.push(None);
                derived_plans.push(None);
                src = RelSrc::Synthetic(si);
            } else {
                // A base table NAME — may resolve to a CTE, which SHADOWS a catalog table of the same
                // name (cte.md §2; case-insensitive). A CTE hit bumps the binding's reference count
                // (the inline-vs-materialize decision — cost.md §3).
                lateral_flags.push(false);
                srf_meta.push(None);
                derived_meta.push(None);
                derived_plans.push(None);
                let lname = tref.name.to_ascii_lowercase();
                src = match ctes.iter().position(|b| b.name == lname) {
                    Some(ci) => {
                        ctes[ci].refs.set(ctes[ci].refs.get() + 1);
                        RelSrc::Cte(&*ctes[ci].table, ci)
                    }
                    None => RelSrc::Base(self.table(&tref.name).ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedTable,
                            format!("table does not exist: {}", tref.name),
                        )
                    })?),
                };
            }
            // RIGHT/FULL JOIN to a CORRELATED lateral item is rejected (§44): the right side cannot be
            // both kept whole and evaluated per left row. (i ≥ 1, so the item carries a join kind.)
            if lateral_flags[i] && matches!(sel.joins[i - 1].kind, JoinKind::Right | JoinKind::Full)
            {
                return Err(EngineError::new(
                    SqlState::InvalidColumnReference,
                    "invalid reference to FROM-clause entry for a LATERAL item: the combining JOIN type must be INNER or LEFT",
                ));
            }
            // The relation's label (alias, else the table/function name; empty for an unaliased derived
            // table, which has no qualifier and never collides). A duplicate explicit label is 42712.
            let table: &Table = match src {
                RelSrc::Base(t) | RelSrc::Cte(t, _) => t,
                RelSrc::Synthetic(idx) => &synthetic[idx],
            };
            let label = tref
                .alias
                .clone()
                .unwrap_or_else(|| table.name.clone())
                .to_ascii_lowercase();
            let col_count = table.columns.len();
            if !label.is_empty() && !seen_labels.insert(label.clone()) {
                return Err(EngineError::new(
                    SqlState::DuplicateAlias,
                    format!("table name {label} specified more than once"),
                ));
            }
            finalized.push(FinalRel { label, offset, src });
            offset += col_count;
        }
        // Assemble the persistent scope: every synthetic table now has a stable address (no more
        // pushes), so `rels` may borrow them.
        let rels: Vec<ScopeRel> = finalized
            .iter()
            .map(|fr| ScopeRel {
                label: fr.label.clone(),
                table: fr.table(&synthetic),
                offset: fr.offset,
                qualifier_only: false,
                cte: match fr.src {
                    RelSrc::Cte(_, ci) => Some(ci),
                    _ => None,
                },
            })
            .collect();
        let scope = Scope {
            rels,
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
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
        // Scan-bound pushdown, per base relation: detect WHERE conjuncts that bound that
        // relation's scan — a PK range, else a secondary-index equality — so it seeks/ranges
        // instead of walking the whole B-tree (cost.md §3 "bounded scan" / "index-bounded
        // scan"; indexes.md §5). The filter is resolved against the full FROM scope, so a
        // relation's column is the GLOBAL index `rel.offset + local`; `const_source` only
        // accepts a literal/param/outer const (never a sibling column), so a JOIN base table is
        // bounded only by a CONSTANT predicate on its own columns — `b.pk = a.x` (the
        // index-nested-loop case) stays a full scan, a follow-on. Sound for outer joins too: a
        // non-NULL conjunct in WHERE eliminates that relation's NULL-extended rows, so bounding
        // it cannot drop a surviving row.
        // A set-returning relation is a computed row source with no PK/index — it never bounds
        // (functions.md §10), so skip detection for it (the synthetic table would return None
        // anyway, but gate it explicitly).
        let rel_bounds: Vec<Option<ScanBound>> = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, rel)| match (&filter, &srf_meta[i], &derived_meta[i]) {
                // A scan bound applies only to a base table — a set-returning function or a derived
                // table is a computed source with no store to seek (functions.md §10, §42).
                (Some(f), None, None) => detect_scan_bound(f, rel),
                _ => None,
            })
            .collect();
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
                        ctes,
                    };
                    Some(resolve_boolean_filter(&partial, on_expr, ptypes)?)
                }
            };
            joins.push(PlanJoin { kind: j.kind, on });
        }

        // Assemble the owned plan (table NAMES + offsets/widths replace the scope's `&Table`s,
        // so the plan outlives the scope and a correlated subquery can re-execute it per row).
        let mut srf_plans: Vec<Option<SrfPlan>> = srf_meta
            .into_iter()
            .map(|m| m.map(|(_, args, kind)| SrfPlan { kind, args }))
            .collect();
        let rels: Vec<PlanRel> = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, r)| PlanRel {
                table_name: r.table.name.clone(),
                offset: r.offset,
                col_count: r.table.columns.len(),
                srf: srf_plans[i].take(),
                cte: r.cte,
                derived: derived_plans[i].take().map(Box::new),
                lateral: lateral_flags[i],
            })
            .collect();
        // The touched set per relation (cost.md §3 "The touched set"; large-values.md §14):
        // the columns this query statically references, collected depth-aware so a correlated
        // subquery's outer reference back into this scope counts. An aggregate query's
        // projections / HAVING / ORDER BY index the synthetic group row, whose inputs are
        // exactly the group keys + aggregate arguments collected here; a plain query's
        // projections and ORDER BY keys index the combined row directly.
        let total_cols: usize = rels.iter().map(|r| r.col_count).sum();
        let mut touched = vec![false; total_cols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut touched);
        }
        for j in &joins {
            if let Some(on) = &j.on {
                collect_touched(on, 0, &mut touched);
            }
        }
        if is_agg {
            for &k in &group_keys {
                touched[k] = true;
            }
            for s in &agg_specs {
                if let Some(op) = &s.operand {
                    collect_touched(op, 0, &mut touched);
                }
            }
        } else {
            for p in &projections {
                collect_touched(p, 0, &mut touched);
            }
            for &(slot, _, _) in &order {
                touched[slot] = true;
            }
        }
        let rel_masks: Vec<Vec<bool>> = rels
            .iter()
            .map(|r| touched[r.offset..r.offset + r.col_count].to_vec())
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
            rel_bounds,
            rel_masks,
        })
    }

    /// Resolve a FROM-clause set-returning function call into a **synthetic one-column relation**
    /// plus its resolved argument expressions and the [`SrfKind`] selecting its generator
    /// (spec/design/functions.md §10, array-functions.md §9). Two SRFs exist: `generate_series`
    /// (2/3 integer args) and the polymorphic `unnest(anyarray)` (1 array arg); any other name →
    /// `42883`. Non-LATERAL: the args resolve against an EMPTY-local-rels scope whose `parent` is
    /// the enclosing query, so `$N` and correlated outer columns resolve while a sibling FROM table
    /// does not (42703/42P01). The produced column's NAME follows PostgreSQL's single-column
    /// function-alias rule: the table alias when one is given (`unnest(xs) AS g` ⇒ column `g`),
    /// else the function name. Returns `(synthetic table, resolved args, kind)`.
    fn resolve_srf<'a>(
        &'a self,
        name: &str,
        args: &[Expr],
        alias: Option<&str>,
        parent: Option<&Scope<'a>>,
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        // The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings
        // are inherited so an arg subquery can reference a CTE (cte.md §2).
        let arg_scope = Scope {
            rels: Vec::new(),
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
        };
        if name.eq_ignore_ascii_case("generate_series") {
            return self.resolve_generate_series(args, alias, &arg_scope, ptypes);
        }
        if name.eq_ignore_ascii_case("unnest") {
            return self.resolve_unnest(args, alias, &arg_scope, ptypes);
        }
        Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("function does not exist: {name}"),
        ))
    }

    /// Resolve `generate_series(start, stop[, step])` (spec/design/functions.md §10): 2 or 3
    /// integer args (a wrong arity/type → `42883`). The produced column is typed at the PROMOTED
    /// integer type of the args (PG); a NULL-typed arg contributes no width (the call yields zero
    /// rows at exec). All-NULL defaults i64.
    fn resolve_generate_series(
        &self,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 2 && args.len() != 3 {
            return Err(no_func_overload("generate_series"));
        }
        let mut rargs = Vec::with_capacity(args.len());
        let mut result: Option<ScalarType> = None;
        for a in args {
            let (r, t) = resolve(
                arg_scope,
                a,
                Some(ScalarType::Int64),
                &mut AggCtx::Forbidden,
                ptypes,
            )?;
            match t {
                ResolvedType::Int(st) => {
                    result = Some(match result {
                        Some(prev) if prev.rank() >= st.rank() => prev,
                        _ => st,
                    });
                }
                ResolvedType::Null => {}
                _ => return Err(no_func_overload("generate_series")),
            }
            rargs.push(r);
        }
        let result = result.unwrap_or(ScalarType::Int64);
        let table = srf_table("generate_series", alias, Type::Scalar(result));
        Ok((table, rargs, SrfKind::GenerateSeries))
    }

    /// Resolve `unnest(anyarray)` (spec/design/array-functions.md §9, §13): the single argument must
    /// be an array (binding `ELEM` := its element type, the produced column's type), else `42883`
    /// (a non-array, e.g. `unnest(5)`). A bare untyped `NULL` argument leaves `ELEM` undeterminable
    /// → `42P18` (jed's polymorphic posture, exactly like `array_append(NULL, NULL)`); a *typed*
    /// NULL array (`NULL::i32[]`) resolves and yields zero rows at exec. `ELEM` may be a **scalar
    /// or a composite** (AF7 — `unnest(composite[])`): the synthetic column is typed at the bound
    /// element type directly (`type_from_resolved`), so a composite array produces composite rows
    /// (an anonymous-composite element has no catalog name → `0A000`, not reachable from a typed array).
    fn resolve_unnest(
        &self,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 1 {
            return Err(no_func_overload("unnest"));
        }
        let (rarg, t) = resolve(arg_scope, &args[0], None, &mut AggCtx::Forbidden, ptypes)?;
        let elem_ty = match t {
            ResolvedType::Array(elem) => type_from_resolved(&elem)?,
            ResolvedType::Null => return Err(indeterminate_poly()),
            _ => return Err(no_func_overload("unnest")),
        };
        let table = srf_table("unnest", alias, elem_ty);
        Ok((table, vec![rarg], SrfKind::Unnest))
    }

    /// Generate the rows of a `generate_series(start, stop[, step])` FROM-clause source
    /// (spec/design/functions.md §10), as a `Vec` of one-column rows. The args evaluate ONCE
    /// against the outer environment with an empty local row (non-LATERAL — they reference only
    /// params/outer). PostgreSQL semantics: any NULL arg → zero rows; a step of zero → `22023`;
    /// `start > stop` with a positive step (or the reverse) → zero rows; an i64 overflow while
    /// stepping STOPS the series cleanly (no trap). Each generated element charges one
    /// `generated_row` AT THE SOURCE, guarded so a `max_cost` ceiling aborts a runaway series
    /// (54P01) mid-generation before the whole thing materializes (CLAUDE.md §13).
    fn generate_series_rows(
        &self,
        srf: &SrfPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let eval_int = |e: &RExpr, m: &mut Meter| -> Result<Option<i64>> {
            match e.eval(&[], env, m)? {
                Value::Int(n) => Ok(Some(n)),
                Value::Null => Ok(None),
                _ => unreachable!("the resolver restricts generate_series args to integers"),
            }
        };
        let start = eval_int(&srf.args[0], meter)?;
        let stop = eval_int(&srf.args[1], meter)?;
        let step = match srf.args.get(2) {
            None => Some(1),
            Some(e) => eval_int(e, meter)?,
        };
        // Any NULL argument yields zero rows (PG).
        let (start, stop, step) = match (start, stop, step) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(Vec::new()),
        };
        if step == 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "step size cannot be equal to zero",
            ));
        }
        let mut out: Vec<Row> = Vec::new();
        let mut cur = start;
        loop {
            let in_range = if step > 0 { cur <= stop } else { cur >= stop };
            if !in_range {
                break;
            }
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            out.push(vec![Value::Int(cur)]);
            // i64 overflow while stepping ends the series cleanly, matching PostgreSQL.
            match cur.checked_add(step) {
                Some(next) => cur = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// Generate the rows of an `unnest(anyarray)` FROM-clause source (spec/design/array-functions.md
    /// §9), as a `Vec` of one-column rows. The single array argument evaluates ONCE against the
    /// outer environment with an empty local row (non-LATERAL). PostgreSQL semantics: a **NULL
    /// array** yields zero rows; the **empty array** `{}` yields zero rows; otherwise one row per
    /// element in **flattened row-major order** (a multidimensional array flattens; a NULL element
    /// is produced as a NULL row). Each produced element charges one `generated_row` AT THE SOURCE,
    /// guarded so a `max_cost` ceiling aborts a runaway unnest (54P01) mid-generation, exactly like
    /// `generate_series` (CLAUDE.md §13).
    fn unnest_rows(&self, srf: &SrfPlan, env: &EvalEnv, meter: &mut Meter) -> Result<Vec<Row>> {
        let arr = match srf.args[0].eval(&[], env, meter)? {
            // A NULL array → zero rows (PG; the `empty_on_null` discipline).
            Value::Null => return Ok(Vec::new()),
            Value::Array(a) => a,
            _ => unreachable!("the resolver restricts unnest's argument to an array"),
        };
        let mut out: Vec<Row> = Vec::with_capacity(arr.elements.len());
        for e in arr.elements {
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            out.push(vec![e]);
        }
        Ok(out)
    }

    /// The LIMIT short-circuit path (spec/design/cost.md §3): a single-table, no-blocking-operator
    /// query with a LIMIT streams scan→filter→project and stops the scan the instant the LIMIT/OFFSET
    /// window is filled, charging storage_row_read only for the rows actually read. Cost-equivalent to
    /// the eager path EXCEPT that it reads (and filters) fewer rows — the deliberate cost change.
    /// page_read is the full block (the bound's node count); only the row reads short-circuit. Rows
    /// match the eager path exactly: the offset..offset+limit slice of the primary-key-ordered
    /// filtered rows.
    fn exec_streaming_limit(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
    ) -> Result<SelectResult> {
        let store = self.store(&plan.rels[0].table_name);

        // Resolve the scan bound (the PK pushdown, if any) and charge the page_read block. This path
        // is single-table (gated below), so the only relation is `rel_bounds[0]`. A correlated bound
        // resolves against `env.outer` (the enclosing rows). An INDEX bound never streams — the
        // dispatch gate routes it to the eager path (cost.md §3 "LIMIT short-circuit").
        let (bound, empty) = match &plan.rel_bounds[0] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, env.outer) {
                Some(b) => (b, false),
                None => (KeyBound::unbounded(), true),
            },
            Some(ScanBound::Index(_)) | Some(ScanBound::Gin(_)) => {
                unreachable!("the streaming path is gated to PK/full scans")
            }
            None => (KeyBound::unbounded(), false),
        };
        let (overlap, slabs) = if empty {
            (0, 0)
        } else {
            store.overlap_scan_units(&bound, &plan.rel_masks[0])?
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);

        let limit = plan.limit.expect("streaming path is gated on a LIMIT");
        let offset = plan.offset.unwrap_or(0);
        let mut out: Vec<Vec<Value>> = Vec::new();
        if !empty && limit > 0 {
            let mut passed: i64 = 0;
            store.scan_range(&bound, &mut |_key, row| {
                meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
                meter.charge(COSTS.storage_row_read);
                // Materialize the touched columns if the lazy load left them unfetched
                // (large-values.md §14); the resolved copy is made only when needed, so the
                // streaming path stays allocation-free for fully-resident rows.
                let resolved;
                let row = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
                    let mut r = row.clone();
                    store.resolve_columns(&mut r, &plan.rel_masks[0])?;
                    resolved = r;
                    &resolved
                } else {
                    row
                };
                let keep = match &plan.filter {
                    Some(f) => f.eval(row, env, meter)?.is_true(),
                    None => true,
                };
                if !keep {
                    return Ok(true);
                }
                passed += 1;
                if passed <= offset {
                    return Ok(true);
                }
                meter.charge(COSTS.row_produced);
                let mut projected = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    projected.push(p.eval(row, env, meter)?);
                }
                out.push(projected);
                Ok((out.len() as i64) < limit) // stop once the window is filled
            })?;
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out,
            cost: meter.accrued,
        })
    }

    /// Streaming external sort for a single-table `ORDER BY` (spec/design/spill.md §4/§5). Streams
    /// scan→filter→[`Sorter`], so the input is never materialized in the executor heap; the sorter
    /// spills sorted runs to disk under `work_mem` (file-backed databases) and k-way-merges them at
    /// `finish`, then the window/projection loop pulls the sorted rows one at a time. Results + cost
    /// are byte-identical to the eager sort: the same `page_read` block, `storage_row_read` per
    /// scanned row, filter `operator_eval`, and `row_produced` per windowed row accrue — only the
    /// sort, which is unmetered (cost.md §3), now spills. Gated (by the caller) to a single table, no
    /// join, non-aggregate, non-DISTINCT, with an `ORDER BY` and no index bound.
    fn exec_streaming_sort(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
    ) -> Result<SelectResult> {
        let store = self.store(&plan.rels[0].table_name);

        // Resolve the scan bound (the PK pushdown, if any) and charge the page_read +
        // value_decompress block up front — identical to the eager scan (cost.md §3). An INDEX
        // bound never reaches here (the dispatch gate routes it to the eager path).
        let (bound, empty) = match &plan.rel_bounds[0] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, env.outer) {
                Some(b) => (b, false),
                None => (KeyBound::unbounded(), true),
            },
            Some(ScanBound::Index(_)) | Some(ScanBound::Gin(_)) => {
                unreachable!("the streaming sort path is gated to PK/full scans")
            }
            None => (KeyBound::unbounded(), false),
        };
        let (overlap, slabs) = if empty {
            (0, 0)
        } else {
            store.overlap_scan_units(&bound, &plan.rel_masks[0])?
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);

        // Stream the scan → filter → sorter. ORDER BY is blocking, so the scan never short-circuits:
        // every in-range row is read (charging storage_row_read), its touched columns resolved
        // (large-values.md §14), the WHERE applied (charging operator_eval), and a survivor pushed
        // into the sorter, which spills when it exceeds the budget. Only surviving rows are cloned.
        let mut sorter = self.new_sorter(&plan.order);
        if !empty {
            store.scan_range(&bound, &mut |_key, row| {
                meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
                meter.charge(COSTS.storage_row_read);
                let resolved = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
                    let mut r = row.clone();
                    store.resolve_columns(&mut r, &plan.rel_masks[0])?;
                    Some(r)
                } else {
                    None
                };
                let row_ref = resolved.as_ref().unwrap_or(row);
                let keep = match &plan.filter {
                    Some(f) => f.eval(row_ref, env, meter)?.is_true(),
                    None => true,
                };
                if keep {
                    sorter.push(resolved.unwrap_or_else(|| row.clone()))?;
                }
                Ok(true) // never stop early — the sort must see every row
            })?;
        }

        // LIMIT / OFFSET window over the sort's total row count (known without materializing the
        // output). Clamp in the i64 domain before indexing (CLAUDE.md §8).
        let total = sorter.total() as i64;
        let start = plan.offset.unwrap_or(0).min(total);
        let end = match plan.limit {
            Some(lim) if lim < total - start => start + lim,
            _ => total,
        };
        let mut sorted = sorter.finish()?;
        for _ in 0..start {
            sorted.next()?; // skip the OFFSET rows (unwindowed — no row_produced)
        }
        let mut out: Vec<Vec<Value>> = Vec::with_capacity((end - start) as usize);
        for _ in start..end {
            let row = sorted
                .next()?
                .expect("the sorter yields exactly `total` rows");
            meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
            meter.charge(COSTS.row_produced);
            let mut projected = Vec::with_capacity(plan.projections.len());
            for p in &plan.projections {
                projected.push(p.eval(&row, env, meter)?);
            }
            out.push(projected);
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out,
            cost: meter.accrued,
        })
    }

    /// Build an [`Sorter`](crate::spill::Sorter) for `order`, bounded by this handle's `work_mem`.
    /// Spilling is enabled only for a **file-backed** database (an in-memory one has nowhere to
    /// spill — spill.md §2); spill runs live next to the database file (same filesystem, guaranteed
    /// writable), falling back to the system temp dir.
    fn new_sorter(&self, order: &[(usize, bool, bool)]) -> crate::spill::Sorter {
        let spill_dir = if self.paging.is_some() {
            let dir = self
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            Some(dir)
        } else {
            None
        };
        crate::spill::Sorter::new(order.to_vec(), self.work_mem, spill_dir)
    }

    /// Materialize one FROM relation `ri` into its rows, given the current outer-row stack `outer`
    /// (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
    /// `outer`); an SRF is generated; a CTE / derived table is delivered / run in place. For a
    /// CORRELATED `LATERAL` relation (§44) the caller passes `outer` EXTENDED with the combined
    /// left-hand row, so the body / SRF args read that row as their immediate outer; a non-lateral
    /// relation is passed the query's own `outer` and its `parent = None` body simply ignores it
    /// (a `parent = None` plan holds no `OuterColumn`, so the two are observably identical).
    fn materialize_rel(
        &self,
        plan: &SelectPlan,
        ri: usize,
        params: &[Value],
        outer: &[&[Value]],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let rel = &plan.rels[ri];
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng,
            ctes,
        };
        // A set-returning relation is generated, not scanned (functions.md §10): produce its rows,
        // charging generated_row per element (its args read `outer` — implicitly lateral, §44).
        if let Some(srf) = &rel.srf {
            return match srf.kind {
                SrfKind::GenerateSeries => self.generate_series_rows(srf, &env, meter),
                SrfKind::Unnest => self.unnest_rows(srf, &env, meter),
            };
        }
        // A CTE reference delivers its rows from the per-statement context (cte.md §3/§5): a
        // MATERIALIZED CTE reads its buffer (charging cte_scan_row, guarded so a runaway scan aborts
        // 54P01); an INLINE CTE runs its body in place. (A CTE is never lateral.)
        if let Some(ci) = rel.cte {
            let rows = match env.ctes.modes[ci] {
                CteMode::Materialize => {
                    let buf = &env.ctes.buffers[ci];
                    for _ in buf {
                        meter.guard()?;
                        meter.charge(COSTS.cte_scan_row);
                    }
                    buf.clone()
                }
                CteMode::Inline => {
                    let r = self.exec_query_plan(&env.ctes.plans[ci], outer, params, env.ctes)?;
                    meter.charge(r.cost);
                    r.rows
                }
            };
            return Ok(rows);
        }
        // A DERIVED TABLE runs its body in place (grammar.md §42), charging its intrinsic cost — no
        // cte_scan_row. Non-lateral it was planned `parent = None` and ignores `outer`; a LATERAL
        // body (§44) reads the left-hand row from `outer`.
        if let Some(dp) = &rel.derived {
            let r = self.exec_query_plan(dp, outer, params, env.ctes)?;
            meter.charge(r.cost);
            return Ok(r.rows);
        }
        // A base table: scan in primary-key order via a ScanSource (the page_read block + per-row
        // storage_row_read accrue inside next() — cost.md §3). A PK/index bound seeks/ranges instead
        // of a full walk; an empty bound reads nothing.
        let store = self.store(&rel.table_name);
        let (mut rows, (node_count, slabs)) = match &plan.rel_bounds[ri] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, outer) {
                Some(b) => {
                    let (entries, pages, slabs) =
                        store.range_scan_with_units(&b, &plan.rel_masks[ri])?;
                    let rows = entries.into_iter().map(|(_, v)| v).collect();
                    (rows, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            Some(ScanBound::Index(ib)) => {
                self.index_bound_rows(&rel.table_name, ib, params, outer, &plan.rel_masks[ri])?
            }
            Some(ScanBound::Gin(gb)) => {
                // Re-find the constant query `Q` in the WHERE filter (the same conjunct the plan-time
                // `gin_match` chose — gin.md §6); the `@>`/`&&` predicate also stays as the residual
                // filter applied to these rows downstream.
                let query = plan
                    .filter
                    .as_ref()
                    .and_then(|f| gin_match(f, gb.col_global).map(|(_, q)| q));
                self.gin_bound_rows(&rel.table_name, gb, query, &env, meter, &plan.rel_masks[ri])?
            }
            None => {
                let (entries, pages, slabs) = store.scan_with_units(&plan.rel_masks[ri])?;
                let rows = entries.into_iter().map(|(_, v)| v).collect();
                (rows, (pages, slabs))
            }
        };
        // Materialize this relation's touched columns where the lazy load left unfetched references
        // (large-values.md §14) — exactly the static set the cost block charges.
        for row in &mut rows {
            store.resolve_columns(row, &plan.rel_masks[ri])?;
        }
        meter.charge(COSTS.value_decompress * slabs as i64);
        let mut src = ScanSource::new(rows, node_count as i64);
        let mut table_rows: Vec<Row> = Vec::new();
        while let Some(row) = src.next(meter)? {
            table_rows.push(row);
        }
        Ok(table_rows)
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
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng: &stmt_rng,
            ctes,
        };
        let mut meter = Meter::with_limit(self.max_cost);

        // LIMIT short-circuit (spec/design/cost.md §3): a single-table query with a LIMIT and no
        // blocking operator (no join, aggregate, DISTINCT, or ORDER BY) streams scan→filter→project
        // and STOPS the scan once the window is filled, so storage_row_read counts only the rows
        // actually read. (ORDER BY/DISTINCT/aggregate must see every row, so they keep the eager path
        // below.) page_read stays the full block; only row reads short-circuit.
        if plan.limit.is_some()
            && plan.rels.len() == 1
            && plan.joins.is_empty()
            && !plan.is_agg
            && !plan.distinct
            && plan.order.is_empty()
            // An index- or GIN-bounded scan does not stream (cost.md §3 "index-bounded scan",
            // gin.md §6): it reads the full admitted set via the eager path below.
            && !matches!(
                plan.rel_bounds[0],
                Some(ScanBound::Index(_)) | Some(ScanBound::Gin(_))
            )
            // A set-returning relation is generated, not scanned — it takes the eager path
            // (functions.md §10); the streaming reader assumes a table store.
            && plan.rels[0].srf.is_none()
            // A CTE reference is a computed/buffered source, not a table store — the eager path
            // (cte.md §5) delivers its rows; the streaming reader assumes a store.
            && plan.rels[0].cte.is_none()
            // A derived table is a computed source too (grammar.md §42) — eager path.
            && plan.rels[0].derived.is_none()
        {
            return self.exec_streaming_limit(plan, &env, &mut meter, params);
        }

        // Streaming external sort (spec/design/spill.md §5): a single-table, no-join,
        // non-aggregate, non-DISTINCT query with an ORDER BY streams scan→filter→Sorter, so the
        // input is never materialized in the executor heap and the sort spills sorted runs to disk
        // under work_mem (file-backed databases). DISTINCT/aggregate/join take the eager path below,
        // and an index bound does not stream (like the LIMIT short-circuit). Results + cost are
        // identical to the eager sort (the sort is unmetered — cost.md §3; spill.md §6).
        if !plan.order.is_empty()
            && plan.rels.len() == 1
            && plan.joins.is_empty()
            && !plan.is_agg
            && !plan.distinct
            && !matches!(
                plan.rel_bounds[0],
                Some(ScanBound::Index(_)) | Some(ScanBound::Gin(_))
            )
            // A set-returning relation takes the eager path (functions.md §10).
            && plan.rels[0].srf.is_none()
            // A CTE reference takes the eager path (cte.md §5).
            && plan.rels[0].cte.is_none()
            // A derived table takes the eager path (grammar.md §42).
            && plan.rels[0].derived.is_none()
        {
            return self.exec_streaming_sort(plan, &env, &mut meter, params);
        }

        // Materialize each relation once, in primary-key order (base tables drain a ScanSource — the
        // page_read block + per-row storage_row_read accrue inside next(), cost.md §3). The nested
        // loop re-reads from these in-memory buffers, which are not stores and charge nothing. A
        // CORRELATED `LATERAL` relation (§44) depends on the left-hand row, so it cannot be
        // materialized up front — a placeholder holds its slot and the join loop re-materializes it
        // per combined left row.
        let mut materialized: Vec<Vec<Row>> = Vec::with_capacity(plan.rels.len());
        for (ri, rel) in plan.rels.iter().enumerate() {
            if rel.lateral {
                materialized.push(Vec::new());
                continue;
            }
            materialized.push(
                self.materialize_rel(plan, ri, params, outer, &stmt_rng, env.ctes, &mut meter)?,
            );
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
        // A FROM-less SELECT has no relations: seed `running` with ONE virtual zero-column row
        // instead of a table's rows (grammar.md §34). No scan ran, so no scan cost accrued.
        let mut running: Vec<Row> = if plan.rels.is_empty() {
            vec![Vec::new()]
        } else {
            std::mem::take(&mut materialized[0])
        };
        for (k, pj) in plan.joins.iter().enumerate() {
            let on = &pj.on;
            let emit_left = matches!(pj.kind, JoinKind::Left | JoinKind::Full);
            let emit_right = matches!(pj.kind, JoinKind::Right | JoinKind::Full);
            // NULL-pad widths come from the PLAN, never a sampled row, so they are correct even
            // when `running`/`right_rows` is empty: the right table begins at flat offset
            // rels[k+1].offset (= the width of every running row) and is that many columns wide.
            let left_pad = plan.rels[k + 1].offset;
            let right_pad = plan.rels[k + 1].col_count;
            let mut next: Vec<Row> = Vec::new();
            // A CORRELATED LATERAL relation (§44): re-materialize it ONCE PER combined left-hand row,
            // with that row pushed onto the outer-row stack as the body's immediate outer (the
            // correlated-subquery mechanism). The plan guarantees INNER/CROSS/LEFT here (RIGHT/FULL
            // to a correlated lateral is 42P10), so there is no unmatched-right emission.
            if plan.rels[k + 1].lateral {
                for left in &running {
                    let mut lat_outer: Vec<&[Value]> = outer.to_vec();
                    lat_outer.push(left);
                    let right_rows = self.materialize_rel(
                        plan,
                        k + 1,
                        params,
                        &lat_outer,
                        &stmt_rng,
                        env.ctes,
                        &mut meter,
                    )?;
                    let mut left_matched = false;
                    for right in &right_rows {
                        let mut combined = left.clone();
                        combined.extend_from_slice(right);
                        let keep = match on {
                            None => true,
                            Some(pred) => pred.eval(&combined, &env, &mut meter)?.is_true(),
                        };
                        if keep {
                            next.push(combined);
                            left_matched = true;
                        }
                    }
                    if emit_left && !left_matched {
                        let mut combined = left.clone();
                        combined.resize(combined.len() + right_pad, Value::Null);
                        next.push(combined);
                    }
                }
                running = next;
                continue;
            }
            let right_rows = &materialized[k + 1];
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
                meter.guard()?; // enforce the cost ceiling per folded row (CLAUDE.md §13)
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
                    groups[gi].1[si].fold(v, &mut meter)?;
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
                meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
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
                meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
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
                meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
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
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        match plan {
            QueryPlan::Select(sp) => self.fold_uncorrelated_in_select(sp, bound, ctes, cost),
            QueryPlan::SetOp(sop) => {
                self.fold_uncorrelated_in_plan(&mut sop.lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_plan(&mut sop.rhs, bound, ctes, cost)
            }
            // A VALUES-body value may itself hold an (uncorrelated) scalar subquery to fold once
            // before the rows are produced (grammar.md §42; the §26 fold).
            QueryPlan::Values(vp) => {
                for row in &mut vp.rows {
                    for e in row {
                        self.fold_uncorrelated_in_rexpr(e, bound, ctes, cost)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn fold_uncorrelated_in_select(
        &self,
        sp: &mut SelectPlan,
        bound: &[Value],
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        for j in &mut sp.joins {
            if let Some(on) = &mut j.on {
                self.fold_uncorrelated_in_rexpr(on, bound, ctes, cost)?;
            }
        }
        if let Some(f) = &mut sp.filter {
            self.fold_uncorrelated_in_rexpr(f, bound, ctes, cost)?;
        }
        if let Some(h) = &mut sp.having {
            self.fold_uncorrelated_in_rexpr(h, bound, ctes, cost)?;
        }
        for s in &mut sp.agg_specs {
            if let Some(op) = &mut s.operand {
                self.fold_uncorrelated_in_rexpr(op, bound, ctes, cost)?;
            }
        }
        for p in &mut sp.projections {
            self.fold_uncorrelated_in_rexpr(p, bound, ctes, cost)?;
        }
        // A set-returning relation's arguments may themselves contain an (uncorrelated) subquery
        // to fold once before the generator runs (functions.md §10).
        for r in &mut sp.rels {
            if let Some(srf) = &mut r.srf {
                for a in &mut srf.args {
                    self.fold_uncorrelated_in_rexpr(a, bound, ctes, cost)?;
                }
            }
        }
        Ok(())
    }

    /// Fold this node if it is an uncorrelated `Subquery`, else recurse into its children.
    fn fold_uncorrelated_in_rexpr(
        &self,
        e: &mut RExpr,
        bound: &[Value],
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        if matches!(e, RExpr::Subquery { .. }) {
            // Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
            // globally-uncorrelated subquery nested inside it is already a constant before we run
            // it. Then leave it untouched if it is correlated (re-run per outer row at eval).
            if let RExpr::Subquery { plan, lhs, .. } = e {
                if let Some(l) = lhs {
                    self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                }
                self.fold_uncorrelated_in_plan(plan, bound, ctes, cost)?;
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
            let r = self.exec_query_plan(&plan, &[], bound, ctes)?;
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
                // An uncorrelated quantified subquery folds to a constant-array `Quantified`
                // (array-functions.md §11.6): its single column becomes a 1-D array and the node
                // reuses the array form's 3VL fold — no per-row re-execution.
                SubqueryKind::Quantified { op, all } => {
                    let elements: Vec<Value> = r
                        .rows
                        .into_iter()
                        .map(|mut row| row.swap_remove(0))
                        .collect();
                    let arr = if elements.is_empty() {
                        ArrayVal::empty()
                    } else {
                        ArrayVal {
                            dims: vec![elements.len()],
                            lbounds: vec![1],
                            elements,
                        }
                    };
                    RExpr::Quantified {
                        op,
                        all,
                        lhs: lhs.expect("a quantified subquery carries its resolved lhs"),
                        array: Box::new(RExpr::ConstArray(Box::new(arr))),
                    }
                }
            };
            return Ok(());
        }
        match e {
            RExpr::Cast { inner, .. } => self.fold_uncorrelated_in_rexpr(inner, bound, ctes, cost),
            RExpr::Neg { operand, .. } => {
                self.fold_uncorrelated_in_rexpr(operand, bound, ctes, cost)
            }
            RExpr::Not(x) => self.fold_uncorrelated_in_rexpr(x, bound, ctes, cost),
            RExpr::Arith { lhs, rhs, .. }
            | RExpr::Compare { lhs, rhs, .. }
            | RExpr::Distinct { lhs, rhs, .. }
            | RExpr::Like { lhs, rhs, .. } => {
                self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(rhs, bound, ctes, cost)
            }
            RExpr::And(l, r) | RExpr::Or(l, r) => {
                self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(r, bound, ctes, cost)
            }
            RExpr::IsNull { operand, .. } => {
                self.fold_uncorrelated_in_rexpr(operand, bound, ctes, cost)
            }
            RExpr::Case { arms, els, .. } => {
                for (c, res) in arms {
                    self.fold_uncorrelated_in_rexpr(c, bound, ctes, cost)?;
                    self.fold_uncorrelated_in_rexpr(res, bound, ctes, cost)?;
                }
                self.fold_uncorrelated_in_rexpr(els, bound, ctes, cost)
            }
            RExpr::ScalarFunc { args, .. }
            | RExpr::ArrayFunc { args, .. }
            | RExpr::Variadic { args, .. } => {
                for a in args {
                    self.fold_uncorrelated_in_rexpr(a, bound, ctes, cost)?;
                }
                Ok(())
            }
            RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
                for f in fields {
                    self.fold_uncorrelated_in_rexpr(f, bound, ctes, cost)?;
                }
                Ok(())
            }
            RExpr::Field { base, .. } => self.fold_uncorrelated_in_rexpr(base, bound, ctes, cost),
            RExpr::Subscript {
                base, subscripts, ..
            } => {
                self.fold_uncorrelated_in_rexpr(base, bound, ctes, cost)?;
                for s in subscripts {
                    match s {
                        RSubscript::Index(i) => {
                            self.fold_uncorrelated_in_rexpr(i, bound, ctes, cost)?
                        }
                        RSubscript::Slice { lower, upper } => {
                            if let Some(l) = lower {
                                self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                            }
                            if let Some(u) = upper {
                                self.fold_uncorrelated_in_rexpr(u, bound, ctes, cost)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            RExpr::InValues { lhs, .. } => self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost),
            RExpr::Quantified { lhs, array, .. } => {
                self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(array, bound, ctes, cost)
            }
            // Leaves and the (already-handled) Subquery: nothing to recurse into.
            RExpr::Subquery { .. }
            | RExpr::Column(_)
            | RExpr::OuterColumn { .. }
            | RExpr::Param(_)
            | RExpr::ConstInt(_)
            | RExpr::ConstBool(_)
            | RExpr::ConstText(_)
            | RExpr::ConstDecimal(_)
            | RExpr::ConstFloat32(_)
            | RExpr::ConstFloat64(_)
            | RExpr::ConstBytea(_)
            | RExpr::ConstUuid(_)
            | RExpr::ConstTimestamp(_)
            | RExpr::ConstTimestamptz(_)
            | RExpr::ConstDate(_)
            | RExpr::ConstInterval(_)
            | RExpr::ConstArray(_)
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

    /// Shared read access to a secondary index's store in the visible snapshot (the index is
    /// known to exist). `name_key` is the lowercased index name.
    fn index_store(&self, name_key: &str) -> &TableStore {
        self.read_snap().index_store(name_key)
    }

    /// Mutable access to a secondary index's store in the working snapshot.
    fn index_store_mut(&mut self, name_key: &str) -> &mut TableStore {
        self.working_mut().index_store_mut(name_key)
    }

    /// Whether the parent currently holds the key/prefix `probe` (committed + working state) — the
    /// child-side foreign-key existence test (spec/design/constraints.md §6.4). `parent_table` is
    /// the referenced table's name. Unmetered, like the PK/UNIQUE probes (cost.md §3).
    fn fk_probe_hits(&self, probe: &FkProbe, parent_table: &str) -> Result<bool> {
        match probe {
            FkProbe::Pk(key) => Ok(self.store(parent_table).get(key)?.is_some()),
            FkProbe::Unique { index, prefix } => Ok(!self
                .index_store(index)
                .range_entries(&unique_probe_bound(prefix))?
                .is_empty()),
        }
    }

    /// Whether any row of `child_table` references the parent tuple `target` (the parent key bytes,
    /// in the byte space [`fk_probe`] produces) via `fk` — the reverse of the child-side probe, a
    /// full scan since child FK columns are not index-backed (spec/design/constraints.md §6.5).
    /// MATCH SIMPLE: a child row with any NULL FK column references nothing. Rows whose storage key
    /// is in `exclude` are skipped — the END STATE for a self-reference, whose child IS the table
    /// being mutated (so its deleted/updated rows must not count). `parent` is the referenced
    /// table's catalog. Unmetered validation.
    fn fk_child_references(
        &self,
        child_table: &str,
        fk: &ForeignKeyConstraint,
        parent: &Table,
        target: &[u8],
        exclude: &HashSet<Vec<u8>>,
    ) -> Result<bool> {
        for (k, row) in self.store(child_table).iter_entries()? {
            if exclude.contains(&k) {
                continue;
            }
            if let Some(probe) = fk_probe(fk, parent, &row, &fk.columns) {
                if probe.bytes() == target {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Every (child table name, FK) pair in the visible snapshot whose FK references `parent_name`
    /// (case-insensitive), including a self-reference — the inbound FKs a parent DELETE/UPDATE must
    /// not strand (spec/design/constraints.md §6.5). Sorted by (lowercased child table, FK name) for
    /// a deterministic report order; cloned so the caller can probe stores without a snapshot borrow.
    fn fk_referencers(&self, parent_name: &str) -> Vec<(String, ForeignKeyConstraint)> {
        let snap = self.read_snap();
        let key = parent_name.to_ascii_lowercase();
        let mut out: Vec<(String, ForeignKeyConstraint)> = Vec::new();
        let mut tkeys: Vec<&String> = snap.tables.keys().collect();
        tkeys.sort();
        for tk in tkeys {
            let t = &snap.tables[tk];
            for fk in &t.foreign_keys {
                if fk.ref_table.eq_ignore_ascii_case(&key) {
                    out.push((t.name.clone(), fk.clone()));
                }
            }
        }
        out
    }

    /// Find the table owning the named index in the visible snapshot (case-insensitive).
    fn find_index(&self, name: &str) -> Option<(&str, &IndexDef)> {
        self.read_snap().find_index(name)
    }

    /// Whether `name` is taken in the shared relation namespace (a table OR an index —
    /// spec/design/indexes.md §2), case-insensitively.
    fn relation_exists(&self, name: &str) -> bool {
        self.table(name).is_some()
            || self.find_index(name).is_some()
            || self.read_snap().sequence(name).is_some()
    }

    /// Choose the auto-generated name for a `serial` column's OWNED sequence (sequences.md §12),
    /// matching PostgreSQL: `lower(table)_lower(column)_seq`, with the smallest integer suffix `1`,
    /// `2`, … appended until the name is free in the relation namespace — not taken by an existing
    /// relation, not equal to the table being created, and not already chosen by an earlier `serial`
    /// column of the same statement (`pending`). All-lowercase identifier-derived, so deterministic.
    fn choose_serial_seq_name(&self, table: &str, column: &str, pending: &[SequenceDef]) -> String {
        let base = format!(
            "{}_{}_seq",
            table.to_ascii_lowercase(),
            column.to_ascii_lowercase()
        );
        let taken = |c: &str| {
            self.relation_exists(c)
                || c.eq_ignore_ascii_case(table)
                || pending.iter().any(|s| s.name.eq_ignore_ascii_case(c))
        };
        if !taken(&base) {
            return base;
        }
        let mut n = 1u32;
        loop {
            let cand = format!("{base}{n}");
            if !taken(&cand) {
                return cand;
            }
            n += 1;
        }
    }

    /// Execute an index equality bound (cost.md §3 "index-bounded scan"): fetch the rows the
    /// equality admits, in index-entry order (= storage-key order among equal values), and
    /// return them with the scan's up-front units `(pages, slabs)` — the index-tree nodes
    /// overlapping the prefix range plus, per admitted entry, the table-tree nodes of that
    /// row's point lookup and its touched-column decompress slabs. The caller feeds the rows
    /// through the same ScanSource as any bounded scan (page_read block + per-row
    /// storage_row_read). A provably empty bound (NULL / contradictory equalities /
    /// out-of-range) returns nothing and charges nothing.
    fn index_bound_rows(
        &self,
        table_name: &str,
        ib: &IndexBound,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
    ) -> Result<(Vec<Row>, (usize, usize))> {
        // Every equality const-source must encode to ONE agreed value: a NULL is 3VL-never-
        // true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
        // integer can equal no stored value — all provably empty.
        let mut agreed: Option<Vec<u8>> = None;
        for src in &ib.eqs {
            let k = match encode_bound_key(ib.col_type, src, params, outer) {
                BoundKey::Null | BoundKey::OutOfRange => return Ok((Vec::new(), (0, 0))),
                BoundKey::Key(k) => k,
            };
            match &agreed {
                None => agreed = Some(k),
                Some(prev) if *prev == k => {}
                Some(_) => return Ok((Vec::new(), (0, 0))),
            }
        }
        // The entry-key prefix: the §2.2 present tag + the value's bare key bytes. The range
        // is every entry extending the prefix: [prefix, byte-successor(prefix)).
        let mut prefix = vec![0x00u8];
        prefix.extend_from_slice(&agreed.expect("an index bound has at least one term"));
        let bound = KeyBound {
            lo: Some(prefix.clone()),
            lo_inc: true,
            hi: prefix_successor(&prefix),
            hi_inc: false,
        };
        let istore = self.index_store(&ib.name_key);
        // The index store has no payload columns, so its mask is empty and its fused scan
        // contributes only the index-tree page_read count (no spill/compress units).
        let (entries, mut pages, _) = istore.range_scan_with_units(&bound, &[])?;
        let store = self.store(table_name);
        let mut slabs = 0usize;
        let mut rows = Vec::with_capacity(entries.len());
        for (ekey, _) in entries {
            // Skip the remaining key components (each self-delimiting — indexes.md §5);
            // the suffix after them is the row's storage key (indexes.md §3).
            let mut at = prefix.len();
            for &ty in &ib.tail_types {
                at += match ekey.get(at) {
                    Some(0x01) => 1,
                    _ => 1 + ty.width_bytes(),
                };
            }
            let row_key = &ekey[at..];
            let (row, n, s) = store.get_with_units(row_key, mask)?;
            pages += n;
            slabs += s;
            rows.push(row.expect("an index entry references a stored row"));
        }
        Ok((rows, (pages, slabs)))
    }

    /// Execute a GIN-bounded scan (spec/design/gin.md §6, cost.md §3). Evaluates the constant
    /// query operand, extracts its terms + mode via the `array_ops` opclass (an array for `@>`/`&&`;
    /// a single scalar term for `= ANY` — `Member`), gathers each term's posting list (a prefix
    /// range scan of the GIN entry tree), combines them by mode (`@>` and `= ANY` → intersection,
    /// `&&` → union) into the candidate storage-key set, and point-looks-up each candidate in
    /// storage-key order. The original predicate stays the residual WHERE filter (re-applied
    /// downstream), so the result is always correct — the bound only narrows which rows are fetched.
    /// Returns the candidate rows + the scan's up-front units `(pages, slabs)` (entry-tree overlap
    /// nodes per term + each candidate's table point-lookup); `gin_entry` (per posting entry
    /// visited) is charged on `meter` directly. Degenerate constant queries (gin.md §6): a NULL `Q`,
    /// an `@>` whose `Q` holds a NULL element, an `&&` with no non-NULL term, and a NULL `= ANY`
    /// scalar are provably empty (read nothing); `@> '{}'` falls back to the full scan.
    fn gin_bound_rows(
        &self,
        table_name: &str,
        gb: &GinBound,
        query: Option<&RExpr>,
        env: &EvalEnv,
        meter: &mut Meter,
        mask: &[bool],
    ) -> Result<(Vec<Row>, (usize, usize))> {
        let store = self.store(table_name);
        // Extract the query's distinct terms. This (the opclass `extract_query_terms`) is a pure
        // planning step, NOT metered (cost.md §3) — evaluate `Q` on a scratch meter. `Q` is a
        // constant, so the empty row suffices.
        let qv = match query {
            Some(q) => q.eval(&[], env, &mut Meter::new())?,
            None => return Ok((Vec::new(), (0, 0))),
        };
        let mut terms: Vec<i64> = Vec::new();
        let mut has_null = false;
        let mut is_empty = false;
        if gb.strategy == GinStrategy::Member {
            // `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL `c` can equal no
            // element, so the bound is provably empty (gin.md §6). `c` is in the element type's
            // range by resolution (jed coerces `c` to the element type, rejecting an out-of-range
            // constant 22003 before exec); the range check is a defensive guard against silently
            // truncating an out-of-range value into a wrong term.
            match &qv {
                Value::Int(n) if *n >= gb.elem_type.min() && *n <= gb.elem_type.max() => {
                    terms.push(*n)
                }
                _ => return Ok((Vec::new(), (0, 0))),
            }
        } else {
            let arr = match &qv {
                // A NULL whole-array query is 3VL-NULL for every row → never TRUE (both @> and &&).
                Value::Null => return Ok((Vec::new(), (0, 0))),
                Value::Array(a) => a,
                _ => return Ok((Vec::new(), (0, 0))), // not an array (impossible post-resolve)
            };
            for el in &arr.elements {
                match el {
                    Value::Int(n) => terms.push(*n),
                    Value::Null => has_null = true,
                    _ => {} // a non-integer element is impossible under the array_ops gate
                }
            }
            is_empty = arr.elements.is_empty();
        }
        terms.sort_unstable();
        terms.dedup();

        match gb.strategy {
            // `@> '{}'`: every non-NULL array contains the empty array — not derivable from the
            // index (which knows only rows that HAVE terms), so fall back to the full scan. The
            // residual filter then keeps the right rows (gin.md §6).
            GinStrategy::Contains if is_empty => {
                let (entries, pages, slabs) = store.scan_with_units(mask)?;
                let rows = entries.into_iter().map(|(_, v)| v).collect();
                return Ok((rows, (pages, slabs)));
            }
            // `@>` a query containing a NULL element is never TRUE (strict element equality).
            GinStrategy::Contains if has_null => return Ok((Vec::new(), (0, 0))),
            // `&&` with no non-NULL term (empty or all-NULL `Q`) overlaps nothing.
            GinStrategy::Overlaps if terms.is_empty() => return Ok((Vec::new(), (0, 0))),
            _ => {}
        }

        // Gather each term's posting list: the entry range [encode(term), successor) of the GIN
        // tree (gin.md §4). The entry is `encode_element(term) ‖ storage_key`; the element type is
        // fixed-width, so the storage key is the suffix after `term_width` bytes.
        let istore = self.index_store(&gb.name_key);
        let term_width = gb.elem_type.width_bytes();
        let mut pages = 0usize;
        let mut entries_visited = 0usize;
        let mut postings: Vec<Vec<Vec<u8>>> = Vec::with_capacity(terms.len());
        for &t in &terms {
            let prefix = encode_int(gb.elem_type, t);
            let bound = KeyBound {
                lo: Some(prefix.clone()),
                lo_inc: true,
                hi: prefix_successor(&prefix),
                hi_inc: false,
            };
            let (es, p, _) = istore.range_scan_with_units(&bound, &[])?;
            pages += p;
            entries_visited += es.len();
            postings.push(
                es.into_iter()
                    .map(|(ekey, _)| ekey[term_width..].to_vec())
                    .collect(),
            );
        }
        meter.charge(COSTS.gin_entry * entries_visited as i64);

        // Combine the posting sets by mode into the candidate storage keys, in ascending byte
        // (= storage-key) order, so the point lookups and the emitted rows follow storage order
        // exactly as a full scan would (gin.md §6/§8).
        let candidates: BTreeSet<Vec<u8>> = match gb.strategy {
            // `@>` ALL → intersection; `= ANY` (Member) is a single term, so its intersection is
            // that lone posting list (gin.md §6).
            GinStrategy::Contains | GinStrategy::Member => {
                let mut it = postings.into_iter();
                let mut acc: BTreeSet<Vec<u8>> =
                    it.next().unwrap_or_default().into_iter().collect();
                for list in it {
                    let s: BTreeSet<Vec<u8>> = list.into_iter().collect();
                    acc.retain(|k| s.contains(k));
                }
                acc
            }
            GinStrategy::Overlaps => postings.into_iter().flatten().collect(),
        };

        let mut slabs = 0usize;
        let mut rows = Vec::with_capacity(candidates.len());
        for key in candidates {
            let (row, n, s) = store.get_with_units(&key, mask)?;
            pages += n;
            slabs += s;
            rows.push(row.expect("a GIN entry references a stored row"));
        }
        Ok((rows, (pages, slabs)))
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
struct ScopeRel<'a> {
    label: String,
    table: &'a Table,
    offset: usize,
    qualifier_only: bool,
    /// `Some(i)` when this relation is a reference to CTE `i` (spec/design/cte.md) rather than a
    /// base table — its `table` is the binding's synthetic relation and exec delivers its rows from
    /// the `CteCtx`. `None` for a base table / SRF / pseudo-relation.
    cte: Option<usize>,
}

/// Where a finalized FROM relation's `&Table` comes from, recorded during the LATERAL-aware FROM
/// build (spec/design/grammar.md §44). A base table / CTE binding has a stable catalog address
/// (`&Table`); a synthetic relation (derived table / SRF) is recorded by INDEX into the local
/// `synthetic` vec — never a borrow — so a record can outlive a later push into that vec, which is
/// what lets a LATERAL item resolve against the earlier synthetic tables while later ones grow.
#[derive(Clone, Copy)]
enum RelSrc<'a> {
    Base(&'a Table),
    Cte(&'a Table, usize),
    Synthetic(usize),
}

/// A FROM relation finalized during the §44 LATERAL-aware build: its label, flat column offset, and
/// table source. Held in FROM order so the prefix `parent` scope a later LATERAL item resolves
/// against (the relations to its left) can be rebuilt, and the persistent scope assembled afterward.
struct FinalRel<'a> {
    label: String,
    offset: usize,
    src: RelSrc<'a>,
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
    catalog: &'s Database,
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
            })
            .collect(),
        parent,
        catalog,
        allow_subquery: true,
        ctes,
    }
}

/// A planned common table expression, owned by `plan_with` for the whole statement (so the scopes
/// that borrow its synthetic `table` outlive it — spec/design/cte.md §A.2). `name` is lowercased
/// for case-insensitive FROM matching; `table` is the synthetic relation exposing the body's output
/// columns; `plan` is the planned body; `hint` is the `[NOT] MATERIALIZED` override; `refs` counts
/// the FROM references resolved to it during planning (a `Cell` — planning borrows `&self`).
struct CteBinding {
    name: String,
    table: Box<Table>,
    plan: QueryPlan,
    hint: Option<bool>,
    refs: std::cell::Cell<usize>,
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
    /// The statement's CTE bindings visible here (spec/design/cte.md §2). Inherited DIRECTLY down
    /// into nested scopes (a subquery sees the same `ctes`), NOT via the `parent` chain — so CTE
    /// lookup never counts as a correlation level. Empty for every non-`WITH` statement.
    ctes: &'a [CteBinding],
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
                qualifier_only: false,
                cte: None,
            }],
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
        }
    }

    /// A column-less scope — the environment a `DEFAULT` expression resolves against
    /// (constraints.md §2): a default may not reference a column (rejected as 0A000 by the
    /// structural pre-walk before resolution) and may not contain a subquery, so there are no
    /// relations and subqueries are disallowed.
    fn empty(catalog: &'a Database) -> Scope<'a> {
        Scope {
            rels: Vec::new(),
            parent: None,
            catalog,
            allow_subquery: false,
            ctes: &[],
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
    fn returning(catalog: &'a Database, table: &'a Table, base_is_old: bool) -> Scope<'a> {
        let n = table.columns.len();
        let label = table.name.to_ascii_lowercase();
        let (old_offset, new_offset) = if base_is_old { (0, n) } else { (n, 0) };
        let mut rels = vec![ScopeRel {
            label: label.clone(),
            table,
            offset: 0,
            qualifier_only: false,
            cte: None,
        }];
        for (pseudo, offset) in [("old", old_offset), ("new", new_offset)] {
            if label != pseudo {
                rels.push(ScopeRel {
                    label: pseudo.to_string(),
                    table,
                    offset,
                    qualifier_only: true,
                    cte: None,
                });
            }
        }
        Scope {
            rels,
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
        }
    }

    /// Resolve a bare column name against THIS scope, then OUTWARD through the parent chain.
    /// Within one scope: two+ relations have it → 42702 ambiguous; exactly one → `Local`; none
    /// → fall through to the parent. A name found only in an ancestor is an `Outer` reference
    /// (nearest scope wins — an inner match shadows an outer one, matching PostgreSQL). 42703
    /// only if no scope in the chain has it. A qualifier-only rel (the RETURNING `old`/`new`
    /// pseudo-relations) is invisible here — no new ambiguity (grammar.md §32).
    fn resolve_bare(&self, name: &str) -> Result<Resolved> {
        let mut found: Option<usize> = None;
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
                if c.name.eq_ignore_ascii_case(name) {
                    if found.is_some() {
                        return Err(ambiguous_column(name));
                    }
                    found = Some(r.offset + local);
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
}

/// The resolved shape of a composite type — its (optional) name and resolved field list. The
/// `name` is `None` for an anonymous `ROW(...)` result, `Some` for a named catalog type.
#[derive(Clone, PartialEq, Eq)]
struct CompositeRType {
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
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
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
    Sin,
    Cos,
    Tan,
    /// make_interval — builds an interval from its (named/defaulted) integer components plus the
    /// f64 `secs` (spec/design/functions.md §11). The one scalar function returning interval.
    MakeInterval,
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
}

/// The polymorphic array functions (spec/design/array-functions.md). Distinct from
/// [`ScalarFunc`] because they resolve over the `anyarray`/`anyelement` pseudo-families (§2) and
/// the builders return an *array* type (not a `ScalarType`), so they get their own resolved node
/// ([`RExpr::ArrayFunc`]). The kernel id is the function name; the eval recovers everything else
/// from the operand values (the array's own shape header), so the node carries no result type.
enum ArrayFunc {
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
}

/// The VARIADIC argument-counting functions (spec/design/array-functions.md §12). Distinct from
/// [`ScalarFunc`] because they are non-strict (`null = "none"`, like [`ArrayFunc`]) and take either
/// a spread of arguments or a single array via the `VARIADIC` keyword — the call form is carried on
/// the [`RExpr::Variadic`] node. Both return `i32`.
enum VariadicFunc {
    /// num_nulls(VARIADIC "any") → i32 — the count of NULL arguments (spread form), or of NULL
    /// flattened elements (VARIADIC-array form; a NULL whole-array operand → NULL). Never NULL in
    /// spread form.
    NumNulls,
    /// num_nonnulls(VARIADIC "any") → i32 — the mirror: the count of non-NULL arguments/elements.
    NumNonnulls,
}

/// One resolved subscript spec in an [`RExpr::Subscript`] (spec/design/array.md §6): a single
/// index `a[i]`, or a slice `a[m:n]` whose bounds may be omitted (`a[:n]`, `a[m:]`, `a[:]`).
enum RSubscript {
    Index(Box<RExpr>),
    Slice {
        lower: Option<Box<RExpr>>,
        upper: Option<Box<RExpr>>,
    },
}

/// A resolved expression: a tree over fixed column indices, ready to evaluate against
/// a row. Arithmetic nodes carry their (promotion-tower) result type so the computed
/// value can be range-checked against it (the i16+i16 → i16 boundary).
enum RExpr {
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
enum SubqueryKind {
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
enum QueryPlan {
    Select(SelectPlan),
    SetOp(Box<SetOpPlan>),
    /// A VALUES-body relation — `FROM (VALUES …) AS v` (spec/design/grammar.md §42): a computed
    /// relation of literal rows, the FROM-position sibling of `INSERT … VALUES`. Only ever produced
    /// as a derived-table body (the parser admits `VALUES` solely there), so it never appears as a
    /// set-op operand or a subquery operand.
    Values(ValuesPlan),
}

impl QueryPlan {
    /// The output column types — for a scalar/IN subquery's plan-time column-count check (42601)
    /// and its folded/element type.
    fn column_types(&self) -> &[ResolvedType] {
        match self {
            QueryPlan::Select(s) => &s.column_types,
            QueryPlan::SetOp(s) => &s.column_types,
            QueryPlan::Values(v) => &v.column_types,
        }
    }

    /// The output column names — the basis for a CTE's synthetic relation when there is no
    /// column-rename list (spec/design/cte.md §1).
    fn column_names(&self) -> &[String] {
        match self {
            QueryPlan::Select(s) => &s.column_names,
            QueryPlan::SetOp(s) => &s.column_names,
            QueryPlan::Values(v) => &v.column_names,
        }
    }
}

/// A resolved VALUES-body relation (spec/design/grammar.md §42), executable to its literal rows.
/// `rows` is the resolved value expressions — `rows[r][c]` is row `r`, column `c` — each resolved
/// as a CONSTANT (the body is non-`LATERAL`, planned `parent = None`, so it reads no row).
/// `column_types` is the per-column type unified across the rows like a set operation (§25), and
/// `column_names` is `column1, column2, …` (PostgreSQL; the derived table's optional column-rename
/// list overrides them at the synthetic relation). All rows have `column_types.len()` values.
struct ValuesPlan {
    rows: Vec<Vec<RExpr>>,
    column_types: Vec<ResolvedType>,
    column_names: Vec<String>,
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
    let body_types = plan.column_types();
    let body_names = plan.column_names();
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
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
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
    })
}

/// One relation in a SELECT plan: the table name (looked up in the store at exec), the flat
/// offset of its first column in the joined row, and its column count (for NULL-padding). When
/// `srf` is `Some`, the relation is a COMPUTED set-returning function (generate_series) rather
/// than a base table: `table_name` is then the function name (never looked up in the store) and
/// the executor generates the rows instead of scanning (spec/design/functions.md §10).
struct PlanRel {
    table_name: String,
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
enum CteMode {
    /// Run the body in place at each reference (re-evaluates per outer row under correlation,
    /// matching PostgreSQL); charges the body's intrinsic cost, no `cte_scan_row`.
    Inline,
    /// Run the body once, buffer the rows; each reference scans the buffer, charging `cte_scan_row`
    /// per buffered row.
    Materialize,
}

/// The per-statement CTE execution context, threaded through `exec_*` and `EvalEnv` so a FROM
/// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). `modes` and
/// `plans` are fixed after planning; `buffers` is filled before the main query runs — one slot per
/// CTE in list order, holding the materialized rows of a `Materialize` CTE (an empty placeholder
/// for an `Inline` one, whose body is run in place from `plans` instead).
#[derive(Clone, Copy)]
struct CteCtx<'a> {
    modes: &'a [CteMode],
    plans: &'a [QueryPlan],
    buffers: &'a [Vec<Row>],
}

impl CteCtx<'_> {
    /// The empty context — no CTEs in scope (every non-`WITH` execution path).
    fn empty() -> CteCtx<'static> {
        CteCtx {
            modes: &[],
            plans: &[],
            buffers: &[],
        }
    }
}

/// Which set-returning function a [`SrfPlan`] is, selecting the row generator at exec
/// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core;
/// the resolution narrows the catalog name to one of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SrfKind {
    /// `generate_series(start, stop[, step])` — an integer series (functions.md §10).
    GenerateSeries,
    /// `unnest(anyarray)` — one row per array element, flattened row-major (array-functions.md §9).
    Unnest,
}

/// A resolved set-returning-function row source (spec/design/functions.md §10, array-functions.md
/// §9). `kind` selects the generator: `generate_series(start, stop[, step])` (`args` = 2 or 3
/// integers) or `unnest(anyarray)` (`args` = the single array expression). Non-LATERAL, so each
/// arg evaluates against the params/outer environment with no local row. The produced column's
/// type lives on the synthetic relation (built in `resolve_srf`), so the plan needs only the
/// resolved arg expressions here.
struct SrfPlan {
    kind: SrfKind,
    args: Vec<RExpr>,
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
            primary_key: false,
            not_null: false,
            default: None,
            default_expr: None,
        }],
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    })
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
    /// Scan-bound pushdown, **one entry per relation** in `rels`: the WHERE conjuncts that
    /// bound that relation's scan — a primary-key range, or (when no PK bound applies) a
    /// secondary-index equality (cost.md §3 "bounded scan" / "index-bounded scan"). `None` ⇒
    /// a full scan of that relation. In a JOIN each base table is bounded independently by
    /// the WHERE predicates against a CONSTANT (literal/param/outer) — a cross-relation
    /// `b.pk = a.x` is the index-nested-loop case (still a follow-on; `const_source` rejects
    /// a sibling column). The residual filter stays the WHOLE `filter`, re-applied after the
    /// join — the bound only narrows which rows are scanned.
    rel_bounds: Vec<Option<ScanBound>>,
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
/// encoding; the PK type disambiguates). `Param` and `Outer` resolve to a value at exec time:
/// `Param` from the bound parameters, `Outer` from an enclosing query's row (a correlated
/// reference — the inner subquery's PK is bounded by the current outer row's column, so it seeks
/// instead of re-scanning the whole inner table per outer row; spec/design/cost.md §3 "bounded
/// scan", grammar.md §26).
enum BoundSrc {
    Int(i64),
    Bool(bool),
    Uuid([u8; 16]),
    Timestamp(i64),
    Date(i32),
    Null,
    Param(usize),
    Outer { level: usize, index: usize },
}

/// One `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is the LEFT side (a
/// `5 < pk` flips to `pk > 5`).
struct BoundTerm {
    op: CmpOp,
    src: BoundSrc,
}

/// The plan-time result of PK analysis: the PK's storage type + the bound terms. The concrete key
/// range is built per execution by `build_key_bound`.
struct PkBound {
    pk_type: ScalarType,
    terms: Vec<BoundTerm>,
}

/// A per-relation scan bound (cost.md §3): a primary-key range, a secondary-index
/// equality (spec/design/indexes.md §5), or a GIN-bounded scan over an array column
/// (spec/design/gin.md §6). The PK bound wins when several apply — it is the row's own key
/// (no second tree, range-capable, strictly cheaper); the ordered-index equality bound wins
/// over GIN (the deterministic precedence, gin.md §6).
enum ScanBound {
    Pk(PkBound),
    Index(IndexBound),
    Gin(GinBound),
}

/// Which array operator a GIN bound accelerates (spec/design/gin.md §6): `@>` (contains, mode
/// ALL → posting-list intersection), `&&` (overlaps, mode ANY → posting-list union), or
/// `= ANY` (membership — `c = ANY(col)`, the single-term `@>` reduction: one scalar term, mode
/// ALL → its lone posting list).
#[derive(Clone, Copy, PartialEq, Eq)]
enum GinStrategy {
    Contains,
    Overlaps,
    /// `c = ANY(col)` — `c` is a constant SCALAR (not an array); its single term is gathered like
    /// a one-element `@>`. The query operand recovered by `gin_match` is the scalar `c`.
    Member,
}

/// The plan-time result of GIN analysis (spec/design/gin.md §6): the chosen GIN index (lowest
/// lowercased name whose array column has a `col @> const` / `col && const` conjunct), the array
/// **element** type (for `encode_element` — the term bytes), the operator strategy, and the
/// column's global scope index. The constant query `Q` is NOT stored (`RExpr` is not `Clone`); it
/// is re-found in `plan.filter` at exec time by `gin_match` and evaluated there.
struct GinBound {
    /// The index store's key — the lowercased index name.
    name_key: String,
    /// The array element type, whose key encoding produces each term's bytes.
    elem_type: ScalarType,
    strategy: GinStrategy,
    /// The GIN-indexed column's global scope index (`rel.offset + ci`).
    col_global: usize,
}

/// The plan-time result of index analysis (indexes.md §5): the chosen index (lowest
/// lowercased name whose FIRST key column has an equality conjunct), that column's storage
/// type, and every equality const-source on it. At exec time the sources must agree on one
/// value (else the bound is provably empty) and the index is range-scanned over that
/// value's presence-tagged prefix.
struct IndexBound {
    /// The index store's key — the lowercased index name.
    name_key: String,
    col_type: ScalarType,
    eqs: Vec<BoundSrc>,
    /// The REMAINING key components' types (`columns[1..]`): an admitted entry's row-key
    /// suffix sits after every component slot, so the fetch skips these (each slot is
    /// self-delimiting — a `0x01` NULL tag alone, or `0x00` + the type's fixed width).
    tail_types: Vec<ScalarType>,
}

/// The outcome of encoding a const-source into the PK key space.
enum BoundKey {
    /// A NULL const — the comparison is 3VL-unknown, so the range is provably empty.
    Null,
    /// An integer value outside the PK type's range — no key can equal it, so drop this half-bound.
    OutOfRange,
    Key(Vec<u8>),
}

/// Pick one relation's scan bound (cost.md §3; indexes.md §5): the single-column PK bound
/// first (the row's own key — range-capable and strictly cheaper); else, among the
/// relation's indexes (held in ascending lowercased-name order — the deterministic
/// tie-break), the first whose FIRST key column has at least one equality conjunct against
/// a type-matched const-source; else `None` (full scan).
fn detect_scan_bound(filter: &RExpr, rel: &ScopeRel) -> Option<ScanBound> {
    if let Some(b) = rel.table.primary_key_index().and_then(|pk_local| {
        detect_pk_bound(
            filter,
            rel.offset + pk_local,
            rel.table.columns[pk_local].ty.scalar(),
        )
    }) {
        return Some(ScanBound::Pk(b));
    }
    for idx in &rel.table.indexes {
        // A GIN index is not an ordered-equality bound — its column is an array and it is keyed by
        // terms, not the whole value (handled by the GIN pass below, gin.md §6).
        if idx.kind == IndexKind::Gin {
            continue;
        }
        let ci = idx.columns[0];
        let ty = rel.table.columns[ci].ty.scalar();
        let mut terms = Vec::new();
        collect_bound_terms(filter, rel.offset + ci, ty, &mut terms);
        let eqs: Vec<BoundSrc> = terms
            .into_iter()
            .filter(|t| matches!(t.op, CmpOp::Eq))
            .map(|t| t.src)
            .collect();
        if !eqs.is_empty() {
            return Some(ScanBound::Index(IndexBound {
                name_key: idx.name.to_ascii_lowercase(),
                col_type: ty,
                eqs,
                tail_types: idx.columns[1..]
                    .iter()
                    .map(|&c| rel.table.columns[c].ty.scalar())
                    .collect(),
            }));
        }
    }
    // GIN bound (gin.md §6) — after the PK and ordered-index equality bounds: the lowest-named GIN
    // index whose array column has a `col @> const` / `col && const` conjunct.
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Gin {
            continue;
        }
        let ci = idx.columns[0];
        let col_global = rel.offset + ci;
        let Some(elem_ty) = rel.table.columns[ci].ty.array_element().map(|t| t.scalar()) else {
            continue; // a GIN column is always an array (the CREATE INDEX gate); defensive
        };
        if let Some((strategy, _)) = gin_match(filter, col_global) {
            return Some(ScanBound::Gin(GinBound {
                name_key: idx.name.to_ascii_lowercase(),
                elem_type: elem_ty,
                strategy,
                col_global,
            }));
        }
    }
    None
}

/// Find the first WHERE AND-chain conjunct that a GIN index on `col_global` accelerates
/// (spec/design/gin.md §6): `col @> Q` (contains) or `col && Q` (overlaps) where `Q` is a
/// **constant** array (references no column / outer / subquery — re-evaluable per scan, not per
/// row). `@>` is asymmetric (the indexed column must be the LEFT operand — `Q @> col` is the
/// non-accelerated `<@`); `&&` is symmetric (the column may be either operand). Returns the
/// strategy and a reference to `Q`. Used both at plan time (for the strategy) and exec time (to
/// recover `Q` from `plan.filter`), so the two agree on the same conjunct by construction.
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
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstNull
        | RExpr::ConstArray(_)
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
        RExpr::Cast { inner, .. } => rexpr_is_constant(inner),
        RExpr::Neg { operand, .. } => rexpr_is_constant(operand),
        RExpr::Not(x) => rexpr_is_constant(x),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::And(lhs, rhs)
        | RExpr::Or(lhs, rhs) => rexpr_is_constant(lhs) && rexpr_is_constant(rhs),
        RExpr::IsNull { operand, .. } => rexpr_is_constant(operand),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .all(|(c, r)| rexpr_is_constant(c) && rexpr_is_constant(r))
                && rexpr_is_constant(els)
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::Variadic { args, .. } => args.iter().all(rexpr_is_constant),
        RExpr::InValues { lhs, .. } => rexpr_is_constant(lhs),
        RExpr::Quantified { lhs, array, .. } => rexpr_is_constant(lhs) && rexpr_is_constant(array),
    }
}

/// A secondary-index entry key (spec/design/indexes.md §3): each indexed column as the
/// encoding.md §2.2 nullable slot — `0x00` + the type's bare order-preserving key bytes when
/// present, the lone `0x01` for NULL (always tagged, even for a NOT NULL column) — then the
/// row's storage key as the suffix. Indexable types are fixed-width and never spill, so the
/// values are always resident (never `Unfetched`).
fn index_entry_key(columns: &[Column], def: &IndexDef, storage_key: &[u8], row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for &ci in &def.columns {
        match &row[ci] {
            Value::Null => out.push(0x01),
            v => {
                out.push(0x00);
                let ty = columns[ci].ty.scalar();
                match v {
                    Value::Int(n) => out.extend_from_slice(&encode_int(ty, *n)),
                    Value::Bool(b) => out.extend_from_slice(&encode_bool(*b)),
                    Value::Uuid(u) => out.extend_from_slice(u),
                    Value::Timestamp(m) | Value::Timestamptz(m) => {
                        out.extend_from_slice(&encode_int(ty, *m))
                    }
                    Value::Date(d) => out.extend_from_slice(&encode_int(ty, *d as i64)),
                    _ => {
                        unreachable!("an index column is a key-encodable type (CREATE INDEX gate)")
                    }
                }
            }
        }
    }
    out.extend_from_slice(storage_key);
    out
}

/// The index entries a row contributes (spec/design/gin.md §4/§5): exactly one for an ordered
/// (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL element for a
/// GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index uniformly as "a
/// row maps to a set of entries."
fn index_entry_keys(
    columns: &[Column],
    def: &IndexDef,
    storage_key: &[u8],
    row: &Row,
) -> Vec<Vec<u8>> {
    match def.kind {
        IndexKind::Btree => vec![index_entry_key(columns, def, storage_key, row)],
        IndexKind::Gin => gin_entries(columns, def, storage_key, row),
    }
}

/// A GIN index's entry keys for one row (spec/design/gin.md §4): one entry per DISTINCT non-NULL
/// array element — `encode_element(term) ‖ storage_key`, with NO presence tag (a term is never
/// NULL) and an empty payload. A NULL array column value and an empty array both yield no entries
/// (so they never appear in any posting list — correct for `@>`/`&&`). Returned sorted by term
/// (= encoded-byte order for the integer element types), so the per-row order is deterministic.
/// This slice: a single integer-element array column (`array_ops`).
fn gin_entries(columns: &[Column], def: &IndexDef, storage_key: &[u8], row: &Row) -> Vec<Vec<u8>> {
    let ci = def.columns[0];
    let elem_ty = columns[ci]
        .ty
        .array_element()
        .expect("a GIN index column is an array (CREATE INDEX gate)")
        .scalar();
    let mut vals: Vec<i64> = Vec::new();
    if let Value::Array(arr) = &row[ci] {
        for el in &arr.elements {
            if let Value::Int(n) = el {
                vals.push(*n);
            }
            // a NULL element (or any non-integer — impossible under the gate) contributes no term
        }
    }
    vals.sort_unstable();
    vals.dedup();
    vals.into_iter()
        .map(|n| {
            let mut entry = encode_int(elem_ty, n);
            entry.extend_from_slice(storage_key);
            entry
        })
        .collect()
}

/// A row's UNIQUENESS PROBE KEY for one unique index (spec/design/indexes.md §8): the §3
/// entry key's slot prefix — without the storage-key suffix — or `None` when any component
/// is NULL (*NULLS DISTINCT*: such a tuple never conflicts). Two rows conflict iff they
/// yield the same `Some` prefix.
fn index_prefix_key(columns: &[Column], def: &IndexDef, row: &Row) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for &ci in &def.columns {
        match &row[ci] {
            Value::Null => return None,
            v => {
                out.push(0x00);
                let ty = columns[ci].ty.scalar();
                match v {
                    Value::Int(n) => out.extend_from_slice(&encode_int(ty, *n)),
                    Value::Bool(b) => out.extend_from_slice(&encode_bool(*b)),
                    Value::Uuid(u) => out.extend_from_slice(u),
                    Value::Timestamp(m) | Value::Timestamptz(m) => {
                        out.extend_from_slice(&encode_int(ty, *m))
                    }
                    Value::Date(d) => out.extend_from_slice(&encode_int(ty, *d as i64)),
                    _ => {
                        unreachable!("an index column is a key-encodable type (CREATE INDEX gate)")
                    }
                }
            }
        }
    }
    Some(out)
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
/// equals a PK/UNIQUE parent column, CREATE TABLE §6.2).
fn encode_key_value(ty: ScalarType, value: &Value) -> Vec<u8> {
    match value {
        Value::Int(n) => encode_int(ty, *n),
        Value::Bool(b) => encode_bool(*b),
        Value::Uuid(u) => u.to_vec(),
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_int(ty, *m),
        Value::Date(d) => encode_int(ty, *d as i64),
        _ => unreachable!("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)"),
    }
}

/// A built foreign-key probe (spec/design/constraints.md §6.4/§6.8): the bytes to look up in the
/// parent, tagged with which physical tree to probe.
enum FkProbe {
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
    row: &Row,
    ordinals: &[usize],
) -> Option<FkProbe> {
    // MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
    if ordinals.iter().any(|&o| matches!(row[o], Value::Null)) {
        return None;
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
    let ref_set = sorted_unique(&fk.ref_columns);
    if !parent.pk.is_empty() && sorted_unique(&parent.pk) == ref_set {
        let mut k = Vec::new();
        for &pcol in &parent.pk {
            let ty = parent.columns[pcol].ty.scalar();
            k.extend_from_slice(&encode_key_value(ty, value_for(pcol)));
        }
        Some(FkProbe::Pk(k))
    } else {
        let idx = parent
            .indexes
            .iter()
            .find(|i| i.unique && sorted_unique(&i.columns) == ref_set)
            .expect("referenced columns matched a unique key at CREATE TABLE §6.2");
        let mut prefix = Vec::new();
        for &pcol in &idx.columns {
            prefix.push(0x00);
            let ty = parent.columns[pcol].ty.scalar();
            prefix.extend_from_slice(&encode_key_value(ty, value_for(pcol)));
        }
        Some(FkProbe::Unique {
            index: idx.name.to_ascii_lowercase(),
            prefix,
        })
    }
}

/// Flatten the WHERE's top-level AND-chain (an OR is never descended — a disjunction is not a
/// contiguous range) and collect every `pk <cmp> const-source` conjunct. `None` ⇒ no usable bound
/// (full scan). Conservative + sound: an unrecognized conjunct contributes no bound and stays in the
/// residual filter.
fn detect_pk_bound(filter: &RExpr, pk_idx: usize, pk_type: ScalarType) -> Option<PkBound> {
    let mut terms = Vec::new();
    collect_bound_terms(filter, pk_idx, pk_type, &mut terms);
    if terms.is_empty() {
        None
    } else {
        Some(PkBound { pk_type, terms })
    }
}

fn collect_bound_terms(e: &RExpr, pk_idx: usize, pk_type: ScalarType, terms: &mut Vec<BoundTerm>) {
    match e {
        RExpr::And(l, r) => {
            collect_bound_terms(l, pk_idx, pk_type, terms);
            collect_bound_terms(r, pk_idx, pk_type, terms);
        }
        // `<>` is not a contiguous range, so it never seeds an index/PK bound — it stays in the
        // residual filter (a full scan + filter). Skipping it here keeps the deterministic cost
        // identical to Go/TS, where `asBoundTerm` excludes it the same way.
        RExpr::Compare { op, lhs, rhs } if !matches!(op, CmpOp::Ne) => {
            let is_pk = |x: &RExpr| matches!(x, RExpr::Column(i) if *i == pk_idx);
            // The PK on either side (op flipped when it is on the right); the other side a
            // matching-type const-source. Anything else contributes no term.
            let term = if is_pk(lhs) {
                const_source(rhs, pk_type).map(|src| BoundTerm { op: *op, src })
            } else if is_pk(rhs) {
                const_source(lhs, pk_type).map(|src| BoundTerm {
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
/// outer row); a column of the same query, arithmetic, etc. are not. A type-mismatched outer
/// reference is wrapped in a `Cast` by the resolver (as for the literal case above), so it never
/// arrives here bare — the type check is implicit and the match stays sound.
fn const_source(e: &RExpr, pk_type: ScalarType) -> Option<BoundSrc> {
    match e {
        RExpr::Param(i) => Some(BoundSrc::Param(*i)),
        RExpr::ConstNull => Some(BoundSrc::Null),
        RExpr::ConstInt(n) if pk_type.is_integer() => Some(BoundSrc::Int(*n)),
        RExpr::ConstBool(b) if pk_type.is_bool() => Some(BoundSrc::Bool(*b)),
        RExpr::ConstUuid(u) if pk_type.is_uuid() => Some(BoundSrc::Uuid(*u)),
        RExpr::ConstTimestamp(m) if pk_type.is_timestamp() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstTimestamptz(m) if pk_type.is_timestamptz() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstDate(d) if pk_type.is_date() => Some(BoundSrc::Date(*d)),
        RExpr::OuterColumn { level, index } => Some(BoundSrc::Outer {
            level: *level,
            index: *index,
        }),
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

/// Build the concrete key range at exec time: encode each const-source and intersect the half-bounds.
/// `outer` carries the enclosing rows (innermost last) so a correlated `Outer` source resolves to
/// the current outer row's value; it is empty for a top-level statement. `None` ⇒ the range admits
/// no key (a NULL const/value — 3VL — or contradictory bounds), so the scan reads nothing. An
/// out-of-range integer const drops only its own half-bound (a wider, still sound, scan).
fn build_key_bound(bp: &PkBound, params: &[Value], outer: &[&[Value]]) -> Option<KeyBound> {
    let mut b = KeyBound::unbounded();
    for t in &bp.terms {
        let key = match encode_bound_key(bp.pk_type, &t.src, params, outer) {
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

/// Encode a const-source's value into the PK's storage key (the same codec INSERT uses — `encode_int`
/// for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte `bool-byte` for boolean).
/// `Param`/`Outer` resolve to a runtime `Value` first (the param table / the enclosing outer row)
/// and then encode through the shared path.
fn encode_bound_key(
    pk_ty: ScalarType,
    src: &BoundSrc,
    params: &[Value],
    outer: &[&[Value]],
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
        BoundSrc::Param(i) => encode_value_key(pk_ty, &params[*i]),
        // A correlated reference: column `index` of the enclosing row `level` hops out — the same
        // indexing the evaluator uses for `RExpr::OuterColumn` (innermost outer row is last).
        BoundSrc::Outer { level, index } => {
            encode_value_key(pk_ty, &outer[outer.len() - level][*index])
        }
    }
}

/// Encode a runtime `Value` (a bound param or a resolved outer column) into the PK's storage key.
/// A NULL value makes the comparison 3VL-unknown (an empty range); a value of a kind no key can
/// hold (or an integer outside the PK width) drops its half-bound, widening — still sound.
fn encode_value_key(pk_ty: ScalarType, v: &Value) -> BoundKey {
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
struct ScanSource {
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

/// The runtime plan for one aggregate, fixed at resolve from the function + operand type
/// (the PG widening — spec/design/aggregates.md §3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AggPlan {
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
    /// SUM(f32|f64) — the ORDER-INDEPENDENT CANONICAL-ORDER FOLD (spec/design/float.md §7).
    /// Carries the width so the result/fold round at the input width. Buffers the finite inputs.
    SumFloat(ScalarType),
    /// AVG(f32|f64) — SUM (canonical fold) / count, one final rounding at the input width.
    AvgFloat(ScalarType),
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
    /// Float SUM/AVG: buffer the canonical inputs (the §7 fold needs ALL values to sort), tracking
    /// NaN / ±Inf presence so the special-value resolution is order-independent. `is_avg` selects
    /// the final SUM vs SUM/count; `width` rounds at the input width. `count` is the non-NULL count.
    FloatFold {
        width: ScalarType,
        is_avg: bool,
        finite: Vec<f64>,
        count: i64,
        any_nan: bool,
        pos_inf: bool,
        neg_inf: bool,
    },
    MinMax {
        cur: Option<Value>,
        is_min: bool,
    },
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
            AggPlan::SumFloat(w) => Acc::FloatFold {
                width: w,
                is_avg: false,
                finite: Vec::new(),
                count: 0,
                any_nan: false,
                pos_inf: false,
                neg_inf: false,
            },
            AggPlan::AvgFloat(w) => Acc::FloatFold {
                width: w,
                is_avg: true,
                finite: Vec::new(),
                count: 0,
                any_nan: false,
                pos_inf: false,
                neg_inf: false,
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
    /// A decimal SUM/AVG fold charges size-scaled `decimal_work` against the running
    /// accumulator (the `+` formula — spec/design/cost.md §3 "decimal_work"); MIN/MAX folds
    /// are direct Value compares like the sort's and stay unmetered.
    fn fold(&mut self, value: Value, m: &mut Meter) -> Result<()> {
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
                    let d = to_decimal(value);
                    m.charge(COSTS.decimal_work * ((decimal::work_linear(sum, &d) - 1) as i64));
                    m.guard()?;
                    // Uncapped: the running sum may exceed the §2 format cap mid-fold; only the
                    // FINAL result is cap-checked (in `finalize`), matching PG and making the trap
                    // order-independent (spec/design/decimal.md §2, determinism.md §7).
                    *sum = sum.add_uncapped(&d);
                    *seen = true;
                }
            }
            Acc::Avg { sum, count } => {
                if !matches!(value, Value::Null) {
                    let d = to_decimal(value);
                    m.charge(COSTS.decimal_work * ((decimal::work_linear(sum, &d) - 1) as i64));
                    m.guard()?;
                    // Uncapped (as SumDecimal): the average's final divide brings the value back in
                    // range, so AVG never traps on an over-cap intermediate sum the way PG does not.
                    *sum = sum.add_uncapped(&d);
                    *count += 1;
                }
            }
            Acc::FloatFold {
                finite,
                count,
                any_nan,
                pos_inf,
                neg_inf,
                ..
            } => {
                // Classify each non-NULL input order-independently (the §7 special-value pass).
                // Convert a f32 to its exact f64 for buffering; the fold re-rounds per step.
                let f = match value {
                    Value::Null => return Ok(()),
                    Value::Float32(f) => f as f64,
                    Value::Float64(f) => f,
                    _ => unreachable!("resolver restricts float SUM/AVG to a float operand"),
                };
                *count += 1;
                if f.is_nan() {
                    *any_nan = true;
                } else if f.is_infinite() {
                    if f > 0.0 {
                        *pos_inf = true;
                    } else {
                        *neg_inf = true;
                    }
                } else {
                    // Canonicalize -0 → +0 before buffering (so the sort/fold are deterministic).
                    finite.push(if f == 0.0 { 0.0 } else { f });
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
                    // The only cap check for the fold: the FINAL sum traps 22003 if over the §2
                    // cap (PG's make_result), but no intermediate does (decimal.md §2).
                    Value::Decimal(sum.check_cap()?)
                } else {
                    Value::Null
                }
            }
            Acc::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    // `div` cap-checks its (in-range) result; the over-cap-capable running `sum` is
                    // never surfaced directly, so AVG matches PG even when SUM would overflow.
                    Value::Decimal(sum.div(&Decimal::from_i64(count))?)
                }
            }
            Acc::FloatFold {
                width,
                is_avg,
                finite,
                count,
                any_nan,
                pos_inf,
                neg_inf,
            } => finalize_float_fold(width, is_avg, finite, count, any_nan, pos_inf, neg_inf)?,
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
        Expr::FuncCall { name, args, .. } => {
            is_aggregate_name(name) || args.iter().any(expr_has_aggregate)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. } => expr_has_aggregate(inner),
        Expr::Unary { operand, .. } => expr_has_aggregate(operand),
        Expr::IsNull { operand, .. } => expr_has_aggregate(operand),
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
        Expr::Like { lhs, rhs, .. } => expr_has_aggregate(lhs) || expr_has_aggregate(rhs),
        Expr::Row(items) | Expr::Array(items) => items.iter().any(expr_has_aggregate),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_aggregate(base),
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
        Expr::Cast { inner, .. } => reject_check_structure(inner),
        Expr::Unary { operand, .. } | Expr::IsNull { operand, .. } => {
            reject_check_structure(operand)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. } => {
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
        Expr::Cast { inner, .. } => reject_default_structure(inner),
        Expr::Unary { operand, .. } | Expr::IsNull { operand, .. } => {
            reject_default_structure(operand)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. } => {
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
            Expr::Cast { inner, .. } => walk(inner, columns, out),
            Expr::Unary { operand, .. } | Expr::IsNull { operand, .. } => {
                walk(operand, columns, out)
            }
            Expr::Binary { lhs, rhs, .. }
            | Expr::IsDistinctFrom { lhs, rhs, .. }
            | Expr::Like { lhs, rhs, .. } => {
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
struct EvalEnv<'a> {
    exec: &'a Database,
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
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::Variadic { args, .. } => args.iter().any(|a| rexpr_references_outer(a, depth)),
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
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
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
            if depth == 0 {
                touched[*i] = true;
            }
        }
        RExpr::OuterColumn { level, index } => {
            if *level == depth && depth > 0 {
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
        RExpr::Cast { inner, .. } => collect_touched(inner, depth, touched),
        RExpr::Neg { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Not(x) => collect_touched(x, depth, touched),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. } => {
            collect_touched(lhs, depth, touched);
            collect_touched(rhs, depth, touched);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            collect_touched(l, depth, touched);
            collect_touched(r, depth, touched);
        }
        RExpr::IsNull { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Case { arms, els, .. } => {
            for (c, r) in arms {
                collect_touched(c, depth, touched);
                collect_touched(r, depth, touched);
            }
            collect_touched(els, depth, touched);
        }
        RExpr::ScalarFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::Variadic { args, .. } => {
            for a in args {
                collect_touched(a, depth, touched);
            }
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
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
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
        ResolvedType::Null => None,
        // A composite/array is no built-in function/aggregate argument family this slice.
        ResolvedType::Composite(_) | ResolvedType::Array(_) => None,
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
        "floor" => ScalarFunc::Floor,
        "trunc" => ScalarFunc::Trunc,
        "sqrt" => ScalarFunc::Sqrt,
        "exp" => ScalarFunc::Exp,
        "ln" => ScalarFunc::Ln,
        "log10" => ScalarFunc::Log10,
        "pow" => ScalarFunc::Pow,
        "sin" => ScalarFunc::Sin,
        "cos" => ScalarFunc::Cos,
        "tan" => ScalarFunc::Tan,
        "make_interval" => ScalarFunc::MakeInterval,
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
        if *slot != "anyarray" && *slot != "anyelement" && !family_matches(slot, t) {
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
        ResolvedType::Null | ResolvedType::Composite(_) | ResolvedType::Array(_) => None,
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
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if matches!(agg, AggCtx::Forbidden) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    let mut sub = AggCtx::Forbidden;
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
        let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
        let desc = lookup_aggregate_overload(name, &t).ok_or_else(|| no_agg_overload(name))?;
        let (plan, result) = aggregate_plan(name, desc.result, &t);
        (plan, Some(r), result)
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
    let ty = resolved_type_of_col(&scope.column_at(idx).ty, scope.catalog);
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

/// Whether `name` (case-insensitive) is a registered aggregate surface (COUNT/SUM/MIN/MAX/AVG).
/// Data-driven over the catalog (`AGGREGATES`); consulted by the grouping + CHECK-structure walks.
fn is_aggregate_name(name: &str) -> bool {
    AGGREGATES
        .iter()
        .any(|a| a.surface.eq_ignore_ascii_case(name))
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
    variadic: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_ascii_lowercase();
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
    // Otherwise the registry (the catalog descriptor tables) decides whether the name is an
    // aggregate, a scalar function, or undefined — no hand-written name lists (extensibility.md §5).
    if is_aggregate_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_aggregate(scope, &lname, args, star, agg, params);
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
    if is_scalar_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_scalar_func(scope, &lname, args, star, agg, params);
    }
    Err(EngineError::new(
        SqlState::UndefinedFunction,
        format!("function does not exist: {name}"),
    ))
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

/// The type context offered to an untyped literal in a function-argument slot of `family`, so it
/// adapts (functions.md §11): an integer slot offers i64, a float slot offers f64 (so a
/// bare `0`/`1.5` becomes f64 for `secs`). Other families offer no hint (the literal keeps
/// its default family, and the slot type-check catches a mismatch).
fn family_hint(family: &str) -> Option<ScalarType> {
    match family {
        "integer" => Some(ScalarType::Int64),
        "float" => Some(ScalarType::Float64),
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
    let func = variadic_func_id(name);

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
        // num_nulls() is 42883). The fixed params match their concrete families; every argument
        // from the variadic slot onward matches the variadic element family ("any" ⇒ all).
        if args.len() < k {
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
    Ok((
        RExpr::Variadic {
            func,
            args: rargs,
            array_form: variadic,
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
            // The RETURNING `old`/`new` pseudo-relations are qualifier-only: `*` expands the
            // real relations' columns exactly as before (grammar.md §32).
            for rel in scope.rels.iter().filter(|r| !r.qualifier_only) {
                for (i, c) in rel.table.columns.iter().enumerate() {
                    nodes.push(RExpr::Column(rel.offset + i));
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
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_) => Err(type_error("argument of WHERE must be boolean")),
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

pub(crate) fn stmt_is_write(stmt: &Statement) -> bool {
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
            w.ctes.iter().any(|c| query_calls_seq_mutator(&c.query))
                || query_calls_seq_mutator(&w.body)
        }
        _ => false,
    }
}

fn query_calls_seq_mutator(qe: &QueryExpr) -> bool {
    match qe {
        QueryExpr::Select(s) => select_calls_seq_mutator(s),
        QueryExpr::SetOp(so) => setop_calls_seq_mutator(so),
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
        || s.group_by.iter().any(expr_calls_seq_mutator)
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
        Expr::Cast { inner, .. } | Expr::Unary { operand: inner, .. } => {
            expr_calls_seq_mutator(inner)
        }
        Expr::IsNull { operand, .. } => expr_calls_seq_mutator(operand),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. } => expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(rhs),
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
        Statement::Begin { .. } => "BEGIN",
        Statement::Commit => "COMMIT",
        Statement::Rollback => "ROLLBACK",
    }
}

/// The resolved (static) type of a column of (possibly composite) declared type `ty`, resolving a
/// composite reference against the database's type catalog (spec/design/composite.md §5). Recurses
/// for nested composites; the lookup always succeeds (`validate_composite_types` proved it).
fn resolved_type_of_col(ty: &Type, db: &Database) -> ResolvedType {
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
            variadic,
        } => {
            let names = arg_names.as_deref().map(Vec::as_slice);
            resolve_func_call(scope, name, args, names, *star, *variadic, agg, params)
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
            let (target, _) = resolve_type_and_typmod(type_name, &None)?;
            coerce_string_literal(text, target, None)
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
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to an array type is only supported from a string literal this slice"
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
            let (target, typmod) = resolve_type_and_typmod(type_name, type_mod)?;
            // A string LITERAL operand is coerced to the target at resolve — `CAST('42' AS int)`,
            // the same primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5).
            // This is the ONLY text→T cast admitted ahead of the general cast slice; a non-literal
            // text operand still falls through to the deferred 0A000 below.
            if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                return coerce_string_literal(s, target, typmod);
            }
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
            // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000
            // (a string lands in an interval column by literal adaptation / the INTERVAL '...'
            // keyword literal, not a CAST).
            if target.is_interval() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to an interval type is not supported yet",
                ));
            }
            // date casts are deferred (spec/design/date.md §5/§6): casting TO date is 0A000 (a
            // string lands in a date column by literal adaptation / the DATE '...' keyword literal,
            // not a CAST).
            if target.is_date() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to a date type is not supported yet",
                ));
            }
            // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int`
            // (and `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
            // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
            // context — its value is range-checked / coerced against `target` at eval — so changing
            // the context only for a parameter leaves all existing CAST behavior untouched.
            let inner_ctx = if matches!(inner.as_ref(), Expr::Param(_)) {
                Some(target)
            } else {
                None
            };
            let (rinner, ity) = resolve(scope, inner, inner_ctx, agg, params)?;
            match ity {
                // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
                // decimal→decimal (re-scale), and NULL are all castable. Floats add int↔float,
                // decimal↔float, and float↔float (spec/design/float.md §6 — all explicit; the
                // eval does the rounding/range-check), so a Float inner is castable too.
                ResolvedType::Int(_)
                | ResolvedType::Decimal
                | ResolvedType::Float(_)
                | ResolvedType::Null => {}
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
                | ResolvedType::Composite(_)
                | ResolvedType::Array(_) => {
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
        Expr::Like { lhs, rhs, negated } => {
            // LIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
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
        BinaryOp::Contains => {
            resolve_containment(scope, lhs, rhs, ArrayFunc::Contains, agg, params)
        }
        BinaryOp::ContainedBy => {
            resolve_containment(scope, lhs, rhs, ArrayFunc::ContainedBy, agg, params)
        }
        BinaryOp::Overlaps => {
            resolve_containment(scope, lhs, rhs, ArrayFunc::Overlaps, agg, params)
        }
    }
}

/// Resolve an array containment/overlap operator `@>` / `<@` / `&&` (array-functions.md §10): a
/// polymorphic `anyarray <op> anyarray → boolean`. Like `resolve_concat` (§8.1) it resolves both
/// operands, adapts a bare literal `ARRAY[…]` to the first array operand's element type, then unifies
/// the two element types over the single `(anyarray, anyarray)` overload — a non-array operand or an
/// element-type mismatch is `42883`. The result is always boolean (so an all-untyped-NULL pair is
/// NOT 42P18 — the type is determinable); the `func` kernel carries the operator. The operators are
/// strict (`null = "propagates"`), so a NULL whole-array operand short-circuits to NULL at eval.
fn resolve_containment(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    func: ArrayFunc,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let no_overload = || {
        EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: the containment/overlap operands are not arrays of a common element type",
        )
    };

    // Pass 1: resolve both operands with no hint.
    let (mut rl, mut lt) = resolve(scope, lhs, None, agg, params)?;
    let (mut rr, mut rt) = resolve(scope, rhs, None, agg, params)?;
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
    match_poly(&["anyarray", "anyarray"], &tys).ok_or_else(no_overload)?;
    Ok((
        RExpr::ArrayFunc {
            func,
            args: vec![rl, rr],
        },
        ResolvedType::Bool,
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
        ResolvedType::Null => None,
        // A composite/array sibling offers no scalar adaptation context.
        ResolvedType::Composite(_) | ResolvedType::Array(_) => None,
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
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_) => {
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
        Array, Bool, Bytea, Composite, Date, Decimal, Float, Int, Interval, Null, Text, Timestamp,
        Timestamptz, Uuid,
    };
    match (lt, rt) {
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
enum Bound {
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
        ResolvedType::Null | ResolvedType::Composite(_) | ResolvedType::Array(_) => None,
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
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_) => Err(type_error(msg)),
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
    catalog: &Database,
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
                    Type::Scalar(scalar) => coerce_string_literal(&s, *scalar, f.decimal)?,
                    // An array-typed field (spec/design/array.md §12): the token is an array text
                    // literal, coerced through `array_in` against the element type — the same path a
                    // bare `'{…}'::T[]` cast uses, one level down. Folds to a constant array.
                    Type::Array(elem_ty) => {
                        let elem_col = resolve_col_type(elem_ty, &catalog.read_snap().types);
                        let val = coerce_string_to_array(&s, &elem_col)?;
                        let rt = resolved_type_of_col(&f.ty, catalog);
                        (value_to_rexpr(&val), rt)
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

fn coerce_string_literal(
    s: &str,
    target: ScalarType,
    typmod: Option<DecimalTypmod>,
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
        // `text 'x'` is identity — the string IS the value.
        ScalarType::Text => (RExpr::ConstText(s.to_string()), ResolvedType::Text),
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
            } else if col_ty.is_interval() {
                // A string literal adapts to an interval column (spec/design/interval.md);
                // malformed input traps 22007, an out-of-range field 22008.
                Ok(Value::Interval(parse_interval(&s)?))
            } else if col_ty.is_date() {
                // A string literal adapts to a date column (spec/design/date.md); malformed
                // input traps 22007, an out-of-range field 22008.
                Ok(Value::Date(parse_date(&s)?))
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
        Value::Date(d) => {
            if col_ty.is_date() {
                Ok(Value::Date(d))
            } else {
                Err(type_error(format!(
                    "cannot store a date value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Interval(iv) => {
            if col_ty.is_interval() {
                Ok(Value::Interval(iv))
            } else {
                Err(type_error(format!(
                    "cannot store an interval value in {} column {col_name}",
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
        // A f32 stores into a f32 column verbatim, or WIDENS losslessly into a f64
        // column (the implicit f32 → f64 cast, spec/types/casts.toml). Other targets 42804.
        Value::Float32(f) => {
            if col_ty.is_float32() {
                Ok(Value::Float32(f))
            } else if col_ty.is_float64() {
                Ok(Value::Float64(f as f64))
            } else {
                Err(type_error(format!(
                    "cannot store a f32 value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // A f64 stores into a f64 column verbatim. f64 → f32 is an EXPLICIT cast
        // (lossy), so it never reaches store_value as an implicit assignment — any other target 42804.
        Value::Float64(f) => {
            if col_ty.is_float64() {
                Ok(Value::Float64(f))
            } else {
                Err(type_error(format!(
                    "cannot store a f64 value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // A composite value into a scalar column is a type mismatch (a composite column routes
        // through `coerce_for_store`/`store_composite`, never the scalar `store_value` — composite.md §4).
        Value::Composite(_) => Err(type_error(format!(
            "cannot store a record value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        Value::Array(_) => Err(type_error(format!(
            "cannot store an array value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        // Poisoned (large-values.md §14): a stored value is an evaluated expression result.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
}

/// Coerce a value into a column for storage, handling **composite** columns (the recursive,
/// field-by-field coercion) as well as scalars (delegating to [`store_value`]). The column's
/// resolved [`ColType`] decides: a scalar column type-checks/range-checks the value as before; a
/// composite column requires a `Value::Composite` of matching arity, coercing each field to its
/// declared field type (recursing for nested composites) — spec/design/composite.md §4.
fn coerce_for_store(
    v: Value,
    ty: &ColType,
    typmod: Option<DecimalTypmod>,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => store_value(v, *s, typmod, not_null, col_name),
        ColType::Composite { name, fields } => store_composite(v, name, fields, not_null, col_name),
        ColType::Array(elem) => store_array(v, elem, not_null, col_name),
    }
}

/// Coerce a value into an **array** column (spec/design/array.md §4): NULL honours NOT NULL
/// (23502); a `Value::Array` coerces each element to the declared element type via
/// [`coerce_for_store`] (a NULL element is allowed — array elements are nullable, so the element
/// store is never NOT NULL); any other value is a 42804.
fn store_array(v: Value, elem: &ColType, not_null: bool, col_name: &str) -> Result<Value> {
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
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.elements.len());
            for val in arr.elements {
                // Elements are nullable (not_null = false); the element typmod is unconstrained
                // this slice (numeric(p,s)[] is deferred — §12).
                out.push(coerce_for_store(val, elem, None, false, col_name)?);
            }
            Ok(Value::Array(ArrayVal {
                dims: arr.dims,
                lbounds: arr.lbounds,
                elements: out,
            }))
        }
        _ => Err(type_error(format!(
            "cannot store a non-array value in array column {col_name}"
        ))),
    }
}

/// Coerce a value into a **composite** column (spec/design/composite.md §4): NULL honours NOT NULL
/// (23502); a `Value::Composite` must have exactly the declared field count (42804) and each field
/// is coerced to its declared field type via [`coerce_for_store`] (recursing); any other value is a
/// 42804. A NULL field of a NOT NULL composite field traps 23502.
fn store_composite(
    v: Value,
    type_name: &str,
    fields: &[ColField],
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
        Value::Composite(vals) => {
            if vals.len() != fields.len() {
                return Err(type_error(format!(
                    "row has {} fields but composite type {type_name} has {}",
                    vals.len(),
                    fields.len()
                )));
            }
            let mut out = Vec::with_capacity(vals.len());
            for (val, f) in vals.into_iter().zip(fields.iter()) {
                out.push(coerce_for_store(val, &f.ty, f.typmod, f.not_null, &f.name)?);
            }
            Ok(Value::Composite(out))
        }
        _ => Err(type_error(format!(
            "cannot store a non-record value in composite column {col_name} (type {type_name})"
        ))),
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

/// Wrap a literal as a runtime value for a given target column type — like [`literal_to_value`],
/// but an integer or decimal literal ADAPTS to a float column (decimal/int → float at the column's
/// width, nearest, round-ties-to-even — spec/design/float.md §4), so `INSERT INTO t(f) VALUES (1.5)`
/// and a `DEFAULT 1.5` on a float column land as floats. An out-of-range magnitude traps 22003 at
/// resolve. Every other literal/target pair falls through unchanged (store_value then type-checks).
fn literal_to_value_for(lit: &Literal, col_ty: ScalarType) -> Result<Value> {
    if col_ty.is_float() {
        match lit {
            Literal::Int(n) => return Ok(int_to_float(*n, col_ty)),
            Literal::Decimal(d) => return decimal_to_float(d, col_ty),
            _ => {}
        }
    }
    Ok(literal_to_value(lit))
}

/// Materialize one INSERT VALUES slot into a `Value` against the column's resolved `ColType`
/// (spec/design/composite.md §1/§4): a scalar slot is a literal (adapted to the type) or a bound
/// `$N`; a composite slot is a `ROW(…)` whose fields recurse against the composite's field types,
/// or a bound `$N`. The result is then fully coerced/range-checked by `coerce_for_store`. `DEFAULT`
/// is handled by the caller at the top level (it is not a valid field inside a `ROW(…)`).
fn materialize_insert_value(iv: &InsertValue, ty: &ColType, bound: &[Value]) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => match iv {
            InsertValue::Lit(lit) => literal_to_value_for(lit, *s),
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Row(_) => Err(type_error(format!(
                "cannot assign a record value to a {} field",
                s.canonical_name()
            ))),
            InsertValue::Array(_) => Err(type_error(format!(
                "cannot assign an array value to a {} field",
                s.canonical_name()
            ))),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ROW(...)",
            )),
        },
        ColType::Composite { name, fields } => match iv {
            InsertValue::Row(field_ivs) => {
                if field_ivs.len() != fields.len() {
                    return Err(type_error(format!(
                        "ROW has {} fields but composite type {name} has {}",
                        field_ivs.len(),
                        fields.len()
                    )));
                }
                let mut vals = Vec::with_capacity(fields.len());
                for (fiv, f) in field_ivs.iter().zip(fields.iter()) {
                    vals.push(materialize_insert_value(fiv, &f.ty, bound)?);
                }
                Ok(Value::Composite(vals))
            }
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Lit(_) => Err(type_error(format!(
                "cannot assign a scalar value to composite column (type {name})"
            ))),
            InsertValue::Array(_) => Err(type_error(format!(
                "cannot assign an array value to composite column (type {name})"
            ))),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ROW(...)",
            )),
        },
        ColType::Array(elem) => match iv {
            // ARRAY[e, …]: a nested constructor (an element is itself `ARRAY[…]`) stacks the
            // sub-arrays into a higher dimension (mirrors the evaluator's `build_nested_array`,
            // spec/design/array.md §4); otherwise each element materializes against the element type
            // into a flat 1-D array. A scalar mixed with an array sub-element errors 42804 (the
            // scalar materialized against the array type), matching PG.
            InsertValue::Array(elem_ivs) => {
                if elem_ivs.iter().any(|e| matches!(e, InsertValue::Array(_))) {
                    let mut subs = Vec::with_capacity(elem_ivs.len());
                    for eiv in elem_ivs {
                        subs.push(materialize_insert_value(eiv, ty, bound)?);
                    }
                    build_nested_array(subs)
                } else {
                    let mut vals = Vec::with_capacity(elem_ivs.len());
                    for eiv in elem_ivs {
                        vals.push(materialize_insert_value(eiv, elem, bound)?);
                    }
                    Ok(Value::Array(ArrayVal::one_dim(vals)))
                }
            }
            // A bare string literal adapts to the array context via `array_in` (the same
            // string-adapts-to-context rule bytea/uuid use — types.md §6; spec/design/array.md §7).
            InsertValue::Lit(Literal::Text(s)) => coerce_string_to_array(s, elem),
            InsertValue::Lit(Literal::Null) => Ok(Value::Null),
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Lit(_) => Err(type_error(
                "cannot assign a scalar value to an array column".to_string(),
            )),
            InsertValue::Row(_) => Err(type_error(
                "cannot assign a record value to an array column".to_string(),
            )),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ARRAY[...]",
            )),
        },
    }
}

/// Parse a text array literal into a `Value::Array` against the element `ColType` via `array_in`
/// (spec/design/array.md §7): each token is coerced to the element type (an unquoted `NULL` token
/// → NULL element). A malformed literal is `22P02`. Used by INSERT (a bare string adapting to an
/// array column) and by the runtime string-literal → array cast.
fn coerce_string_to_array(s: &str, elem: &ColType) -> Result<Value> {
    let parsed = crate::value::parse_array_literal(s).map_err(|e| match e {
        crate::value::ArrayInError::Malformed => EngineError::new(
            SqlState::InvalidTextRepresentation,
            "malformed array literal".to_string(),
        ),
        // An inverted [l:u] bound (`u < l`) — PG `2202E`.
        crate::value::ArrayInError::BoundFlip => {
            array_subscript_err("upper bound cannot be less than lower bound")
        }
    })?;
    let mut elements = Vec::with_capacity(parsed.tokens.len());
    for tok in parsed.tokens {
        match tok {
            None => elements.push(Value::Null),
            // Coerce the token to the element type (a scalar via the string-literal coercion, a
            // composite via record_in — array-of-composite, spec/design/array.md §12 AC1).
            Some(t) => elements.push(coerce_array_element_text(&t, elem)?),
        }
    }
    Ok(Value::Array(ArrayVal {
        dims: parsed.dims,
        lbounds: parsed.lbounds,
        elements,
    }))
}

/// Coerce one array-element text token to a `Value` against the element `ColType` (the `array_in`
/// per-element step, spec/design/array.md §7): a scalar via the same string-literal coercion the
/// scalar typed-literal path uses; a **composite** element via `record_in` (recursive — the
/// array-of-composite quoting nests, §12 AC1 / §7). Self-contained over the resolved `ColType`, so
/// no catalog re-walk (the [`ColType`] design intent). A nested-array element token would recurse,
/// but array-of-array is not a jed type, so it is unreachable in v1.
fn coerce_array_element_text(tok: &str, elem: &ColType) -> Result<Value> {
    match elem {
        ColType::Scalar(s) => {
            let (node, _) = coerce_string_literal(tok, *s, None)?;
            rexpr_const_to_value(&node)
        }
        ColType::Composite { name, fields } => coerce_record_text(tok, name, fields),
        ColType::Array(inner) => coerce_string_to_array(tok, inner),
    }
}

/// `record_in` over a self-contained composite `ColType` (the inverse of `record_out`): the token is
/// the composite's own `(f1,f2,…)` text, tokenized by the shared `value::parse_record_tokens` and
/// recursively coerced per field. Mirrors [`coerce_string_to_composite`] but produces a `Value`
/// directly and walks `ColType` (so it needs no `Database`). A bad shape / field count is `22P02`.
fn coerce_record_text(
    text: &str,
    name: &str,
    fields: &[crate::catalog::ColField],
) -> Result<Value> {
    let malformed = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("malformed record literal: \"{text}\" for type {name}"),
        )
    };
    let tokens = crate::value::parse_record_tokens(text).ok_or_else(malformed)?;
    if tokens.len() != fields.len() {
        return Err(malformed());
    }
    let mut vals = Vec::with_capacity(tokens.len());
    for (tok, f) in tokens.into_iter().zip(fields.iter()) {
        match tok {
            None => vals.push(Value::Null),
            Some(s) => vals.push(match &f.ty {
                ColType::Scalar(sc) => {
                    let (node, _) = coerce_string_literal(&s, *sc, f.typmod)?;
                    rexpr_const_to_value(&node)?
                }
                ColType::Composite {
                    name: n2,
                    fields: f2,
                } => coerce_record_text(&s, n2, f2)?,
                ColType::Array(inner) => coerce_string_to_array(&s, inner)?,
            }),
        }
    }
    Ok(Value::Composite(vals))
}

/// Extract the `Value` from a constant `RExpr` (the const nodes `coerce_string_literal` produces).
fn rexpr_const_to_value(node: &RExpr) -> Result<Value> {
    Ok(match node {
        RExpr::ConstNull => Value::Null,
        RExpr::ConstInt(n) => Value::Int(*n),
        RExpr::ConstBool(b) => Value::Bool(*b),
        RExpr::ConstText(s) => Value::Text(s.clone()),
        RExpr::ConstDecimal(d) => Value::Decimal(d.clone()),
        RExpr::ConstFloat32(f) => Value::Float32(*f),
        RExpr::ConstFloat64(f) => Value::Float64(*f),
        RExpr::ConstBytea(b) => Value::Bytea(b.clone()),
        RExpr::ConstUuid(u) => Value::Uuid(*u),
        RExpr::ConstTimestamp(m) => Value::Timestamp(*m),
        RExpr::ConstTimestamptz(m) => Value::Timestamptz(*m),
        RExpr::ConstDate(d) => Value::Date(*d),
        RExpr::ConstInterval(iv) => Value::Interval(*iv),
        _ => return Err(type_error("non-constant array element literal".to_string())),
    })
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
        // Enforce the cost ceiling before evaluating this node (CLAUDE.md §13). `eval` recurses
        // once per expression node, so guarding here bounds a pathological expression to ~O(1)
        // overshoot; it is a no-op when no ceiling is set (spec/design/cost.md §6).
        m.guard()?;
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
            // A ROW(...) constructor — one operator_eval, then build the composite from the
            // evaluated fields (spec/design/composite.md §1, cost.md §9).
            RExpr::Row(fields) => {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(fields.len());
                for f in fields {
                    vals.push(f.eval(row, env, m)?);
                }
                Ok(Value::Composite(vals))
            }
            // An ARRAY[…] constructor — one operator_eval, then evaluate each element (already
            // coerced to the element type at resolve). A `nested` constructor stacks its sub-arrays
            // into one higher dimension (spec/design/array.md §4); otherwise it is a flat 1-D array.
            RExpr::Array { elems, nested } => {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    vals.push(e.eval(row, env, m)?);
                }
                if *nested {
                    build_nested_array(vals)
                } else {
                    Ok(Value::Array(ArrayVal::one_dim(vals)))
                }
            }
            // A folded array constant (shape preserved) — return it directly.
            RExpr::ConstArray(a) => Ok(Value::Array((**a).clone())),
            // Field selection — one operator_eval, then pull the resolved field ordinal out of the
            // evaluated composite. A whole-value-NULL composite yields NULL (PG); the index is in
            // range by construction (resolve fixed it against the static field list).
            RExpr::Field { base, index } => {
                m.charge(COSTS.operator_eval);
                match base.eval(row, env, m)? {
                    Value::Composite(fields) => Ok(fields[*index].clone()),
                    Value::Null => Ok(Value::Null),
                    other => {
                        unreachable!("field access on a non-composite value: {other:?}")
                    }
                }
            }
            // Array subscript `base[..][..]` (spec/design/array.md §6) — one operator_eval. A NULL
            // array or any NULL subscript bound yields NULL. Element access (`!is_slice`) returns
            // the element when the subscript count equals `ndim` and every index is in range, else
            // NULL; slice access returns a (renumbered) sub-array, with a scalar index `i` meaning
            // `1:i`. The per-element walk is internal (unmetered, cost.md §9).
            // Array subscript — extracted to a free function so its locals do not widen `eval`'s
            // (debug-build) stack frame on the deep-expression path.
            RExpr::Subscript {
                base,
                subscripts,
                is_slice,
            } => {
                m.charge(COSTS.operator_eval);
                eval_subscript(base, subscripts, *is_slice, row, env, m)
            }
            RExpr::ConstInt(n) => Ok(Value::Int(*n)),
            RExpr::ConstBool(b) => Ok(Value::Bool(*b)),
            RExpr::ConstText(s) => Ok(Value::Text(s.clone())),
            RExpr::ConstDecimal(d) => Ok(Value::Decimal(d.clone())),
            RExpr::ConstFloat32(f) => Ok(Value::Float32(*f)),
            RExpr::ConstFloat64(f) => Ok(Value::Float64(*f)),
            RExpr::ConstBytea(b) => Ok(Value::Bytea(b.clone())),
            RExpr::ConstUuid(u) => Ok(Value::Uuid(*u)),
            RExpr::ConstTimestamp(m) => Ok(Value::Timestamp(*m)),
            RExpr::ConstTimestamptz(m) => Ok(Value::Timestamptz(*m)),
            RExpr::ConstDate(d) => Ok(Value::Date(*d)),
            RExpr::ConstInterval(iv) => Ok(Value::Interval(*iv)),
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
                        } else if target.is_float() {
                            // int → float (explicit; nearest, round-ties-to-even — Rust `as`).
                            // Never overflows: i64::MAX < f32::MAX, so the result is always finite.
                            Ok(int_to_float(n, *target))
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
                        } else if target.is_float() {
                            // decimal → float (explicit; nearest binary value). A magnitude that
                            // overflows the float range → 22003 (not ±Inf — the §3 finite rule).
                            decimal_to_float(&d, *target)
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
                    // float → int / decimal / float (all explicit — spec/design/float.md §6).
                    Value::Float32(f) => cast_from_float(f as f64, *target, *typmod),
                    Value::Float64(f) => cast_from_float(f, *target, *typmod),
                    Value::Bool(_) => unreachable!("resolver rejects a boolean cast operand"),
                    Value::Text(_) => unreachable!("resolver rejects a text cast operand"),
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea cast operand"),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid cast operand"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp cast operand")
                    }
                    Value::Date(_) => unreachable!("resolver rejects a date cast operand"),
                    Value::Interval(_) => unreachable!("resolver rejects an interval cast operand"),
                    Value::Composite(_) => {
                        unreachable!("resolver rejects a composite cast operand this slice")
                    }
                    Value::Array(_) => {
                        unreachable!("resolver rejects an array cast operand this slice")
                    }
                    Value::Unfetched(_) => {
                        panic!("BUG: unfetched large value escaped the storage layer")
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
                    // Unary minus flips the float sign bit (no overflow; a NaN/Inf operand passes
                    // through — spec/design/float.md §5). Width preserved by the resolver's result.
                    Value::Float32(f) => Ok(Value::Float32(-f)),
                    Value::Float64(f) => Ok(Value::Float64(-f)),
                    Value::Bool(_) => unreachable!("resolver rejects a boolean unary minus"),
                    Value::Text(_) => unreachable!("resolver rejects a text unary minus"),
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea unary minus"),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid unary minus"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp unary minus")
                    }
                    Value::Date(_) => unreachable!("resolver rejects a date unary minus"),
                    Value::Interval(iv) => Ok(Value::Interval(iv.neg()?)),
                    Value::Composite(_) => {
                        unreachable!("resolver rejects a composite unary minus")
                    }
                    Value::Array(_) => {
                        unreachable!("resolver rejects an array unary minus")
                    }
                    Value::Unfetched(_) => {
                        panic!("BUG: unfetched large value escaped the storage layer")
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
                if result.is_interval() && matches!(op, ArithOp::Mul | ArithOp::Div) {
                    // interval ×÷ number → interval (the exact cascade; spec/design/interval.md
                    // §5). Mul commutes; Div is interval / number (the resolver guarantees the
                    // interval is the left operand). A zero divisor traps 22012.
                    let (iv, num) = match (a, b) {
                        (Value::Interval(iv), n) | (n, Value::Interval(iv)) => (iv, n),
                        _ => unreachable!("resolver guarantees an interval ×÷ number pair"),
                    };
                    let (fnum, fden) = factor_to_fraction(&num)?;
                    let (fnum, fden) = if matches!(op, ArithOp::Mul) {
                        (fnum, fden)
                    } else if fnum == 0 {
                        return Err(EngineError::new(
                            SqlState::DivisionByZero,
                            "division by zero",
                        ));
                    } else if fnum < 0 {
                        (-fden, -fnum) // interval / number = interval * (den/num); keep fden > 0
                    } else {
                        (fden, fnum)
                    };
                    Ok(Value::Interval(crate::interval::mul_by_fraction(
                        &iv, fnum, fden,
                    )?))
                } else if result.is_interval() {
                    // interval ± interval → interval; timestamp[tz] − timestamp[tz] → interval
                    // (spec/design/interval.md §5). Dispatch on the operand kinds.
                    match (a, b) {
                        (Value::Interval(x), Value::Interval(y)) => {
                            let r = match op {
                                ArithOp::Add => x.add(&y)?,
                                ArithOp::Sub => x.sub(&y)?,
                                _ => unreachable!("resolver allows only interval ±"),
                            };
                            Ok(Value::Interval(r))
                        }
                        (Value::Timestamp(x), Value::Timestamp(y))
                        | (Value::Timestamptz(x), Value::Timestamptz(y)) => {
                            Ok(Value::Interval(crate::interval::ts_diff(x, y)?))
                        }
                        _ => unreachable!("resolver guarantees a temporal-difference pair here"),
                    }
                } else if result.is_timestamp() || result.is_timestamptz() {
                    // timestamp[tz] ± interval → timestamp[tz] (calendar month-add with clamping;
                    // spec/design/interval.md §5). interval + timestamp commutes.
                    let subtract = matches!(op, ArithOp::Sub);
                    let (t, iv, is_tz) = match (a, b) {
                        (Value::Timestamp(t), Value::Interval(iv)) => (t, iv, false),
                        (Value::Interval(iv), Value::Timestamp(t)) => (t, iv, false),
                        (Value::Timestamptz(t), Value::Interval(iv)) => (t, iv, true),
                        (Value::Interval(iv), Value::Timestamptz(t)) => (t, iv, true),
                        _ => unreachable!("resolver guarantees a timestamp ± interval pair here"),
                    };
                    let r = crate::interval::ts_shift(t, &iv, subtract)?;
                    Ok(if is_tz {
                        Value::Timestamptz(r)
                    } else {
                        Value::Timestamp(r)
                    })
                } else if result.is_decimal() {
                    // Decimal arithmetic: widen any integer operand to decimal, then apply the
                    // op with PG's scale rules (spec/design/decimal.md §4). The size-scaled
                    // decimal_work is charged BEFORE the operation runs, so a cost ceiling
                    // aborts ahead of the limb work (spec/design/cost.md §3 "decimal_work").
                    let (da, db) = (to_decimal(a), to_decimal(b));
                    let w = decimal_arith_work(*op, &da, &db);
                    m.charge(COSTS.decimal_work * ((w - 1) as i64));
                    m.guard()?;
                    eval_decimal_arith(*op, da, db)
                } else if result.is_float() {
                    // Float arithmetic (spec/design/float.md §5): the IEEE correctly-rounded op at
                    // the result width, ONE op per node (no FMA fusion — the tree-walk guarantees
                    // it). The resolver promoted a mixed-width pair to f64, so both operands
                    // are already the result width. A finite overflow to ±Inf traps 22003, x/0
                    // traps 22012; an Inf/NaN operand propagates by IEEE.
                    match (a, b) {
                        (Value::Float32(x), Value::Float32(y)) => eval_float32_arith(*op, x, y),
                        (Value::Float64(x), Value::Float64(y)) => eval_float64_arith(*op, x, y),
                        _ => unreachable!("resolver promotes float arithmetic to one width"),
                    }
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
                // A decimal(-promotable) pair charges size-scaled decimal_work — once per
                // node, even where `<=`/`>=` decompose internally (cost.md §3 "decimal_work").
                m.charge(COSTS.decimal_work * ((decimal_cmp_work(&a, &b) - 1) as i64));
                m.guard()?;
                let tv = match op {
                    CmpOp::Eq => a.eq3(&b),
                    CmpOp::Ne => a.eq3(&b).not(),
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
                // IS [NOT] NULL is always a definite boolean, never unknown (CLAUDE.md §4). For a
                // composite operand this is PG's recursive all-fields rule (NOT a negation —
                // spec/design/composite.md §5); a scalar follows the ordinary rule. `is_null_test`
                // unifies both.
                let v = operand.eval(row, env, m)?;
                Ok(Value::Bool(v.is_null_test(*negated)))
            }
            RExpr::Distinct { lhs, rhs, negated } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                let rv = rhs.eval(row, env, m)?;
                // IS [NOT] DISTINCT FROM is a comparison: a decimal pair charges its
                // size-scaled decimal_work like Compare (cost.md §3 "decimal_work").
                m.charge(COSTS.decimal_work * ((decimal_cmp_work(&lv, &rv) - 1) as i64));
                m.guard()?;
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
                        // boundary (abs(i16 -32768) → 22003), exactly like Neg.
                        Value::Int(n) => {
                            let v = n.checked_abs().ok_or_else(|| overflow(*result))?;
                            if result.in_range(v) {
                                Ok(Value::Int(v))
                            } else {
                                Err(overflow(*result))
                            }
                        }
                        Value::Decimal(d) => Ok(Value::Decimal(d.abs())),
                        // abs over a float keeps the operand width (NaN passes through; |±Inf| = Inf).
                        Value::Float32(f) => Ok(Value::Float32(f.abs())),
                        Value::Float64(f) => Ok(Value::Float64(f.abs())),
                        _ => unreachable!("resolver restricts abs to numeric operands"),
                    },
                    // round over a float (1- or 2-arg) → f64 (half-away — the engine's mode;
                    // a NaN/Inf operand passes through). Distinguished from decimal round by the
                    // operand variant.
                    ScalarFunc::Round if matches!(&vals[0], Value::Float64(_)) => {
                        let f = match &vals[0] {
                            Value::Float64(f) => *f,
                            _ => unreachable!(),
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts round's count to integer"),
                        };
                        Ok(Value::Float64(round_f64_places(f, places)))
                    }
                    ScalarFunc::Round => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => {
                                unreachable!("resolver restricts round to numeric operands")
                            }
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts round's count to integer"),
                        };
                        Ok(Value::Decimal(d.round_places(places)?))
                    }
                    // The other float functions all take a single f64 arg (the resolver widened
                    // it) and return f64 (spec/design/float.md §8). EXACT (in-contract):
                    // ceil/floor/trunc/sqrt. sqrt of a negative is a DOMAIN error → 22003 (NaN stays
                    // input-only). TRANSCENDENTAL (exempted — native libm): exp/ln/log10/pow/sin/
                    // cos/tan; ln(0)/ln(neg) → 22003, exp/pow overflow → 22003.
                    ScalarFunc::Ceil
                    | ScalarFunc::Floor
                    | ScalarFunc::Trunc
                    | ScalarFunc::Sqrt
                    | ScalarFunc::Exp
                    | ScalarFunc::Ln
                    | ScalarFunc::Log10
                    | ScalarFunc::Pow
                    | ScalarFunc::Sin
                    | ScalarFunc::Cos
                    | ScalarFunc::Tan => {
                        let x = match &vals[0] {
                            Value::Float64(f) => *f,
                            _ => unreachable!("resolver widens a float function arg to f64"),
                        };
                        eval_float_func(*func, x, vals.get(1))
                    }
                    // make_interval — six integer components plus the f64 `secs`. years/
                    // months → months field (×12), weeks/days → days field (×7), hours/mins/secs
                    // → micros; an i32/i64 field overflow traps 22008 (functions.md §11). The one
                    // float step (secs → micros) is correctly-rounded + deterministic, so the
                    // resulting interval is in-contract (not an `R`-exempt float).
                    ScalarFunc::MakeInterval => {
                        let geti = |k: usize| match &vals[k] {
                            Value::Int(n) => *n,
                            _ => unreachable!(
                                "resolver restricts make_interval's components to integers"
                            ),
                        };
                        let secs = match &vals[6] {
                            Value::Float64(f) => *f,
                            // f32 widens losslessly to f64 (every binary32 is an exact binary64).
                            Value::Float32(f) => *f as f64,
                            _ => unreachable!("resolver restricts make_interval's secs to a float"),
                        };
                        let sec_micros = f64_to_micros(secs)?;
                        let iv = interval::make_interval(
                            geti(0),
                            geti(1),
                            geti(2),
                            geti(3),
                            geti(4),
                            geti(5),
                            sec_micros,
                        )?;
                        Ok(Value::Interval(iv))
                    }
                    // uuid extractors (spec/design/functions.md §12): pure bit inspection. Both
                    // return NULL (Value::Null) for a non-RFC variant; the timestamp also for any
                    // version other than 1/7. The NULL-input case is already handled above.
                    ScalarFunc::UuidExtractVersion => match &vals[0] {
                        Value::Uuid(b) => {
                            Ok(crate::uuid::extract_version(b).map_or(Value::Null, Value::Int))
                        }
                        _ => unreachable!("resolver restricts uuid_extract_version to a uuid"),
                    },
                    ScalarFunc::UuidExtractTimestamp => match &vals[0] {
                        Value::Uuid(b) => Ok(crate::uuid::extract_timestamp_micros(b)
                            .map_or(Value::Null, Value::Timestamptz)),
                        _ => unreachable!("resolver restricts uuid_extract_timestamp to a uuid"),
                    },
                    // uuid generators (spec/design/entropy.md §3): draw from the per-statement seam
                    // (a Cell on EvalEnv — interior mutability), advancing the PRNG/counter. The
                    // NULL-arg case (uuidv7(NULL)) already returned NULL above.
                    ScalarFunc::Uuidv4 => {
                        let mut r = env.rng.get();
                        let b = r.uuid_v4(&env.exec.seam)?;
                        env.rng.set(r);
                        Ok(Value::Uuid(b))
                    }
                    ScalarFunc::Uuidv7 => {
                        let mut r = env.rng.get();
                        let clock = r.statement_clock_micros(&env.exec.seam);
                        // The optional interval arg shifts the embedded instant via the existing
                        // calendar-aware timestamptz arithmetic (entropy.md §4).
                        let shifted = match vals.first() {
                            Some(Value::Interval(iv)) => {
                                crate::interval::ts_shift(clock, iv, false)?
                            }
                            Some(_) => {
                                unreachable!("resolver restricts uuidv7's arg to an interval")
                            }
                            None => clock,
                        };
                        let b = r.uuid_v7(&env.exec.seam, shifted)?;
                        env.rng.set(r);
                        Ok(Value::Uuid(b))
                    }
                    // current-time functions (spec/design/entropy.md §5): now() reads the statement
                    // clock ONCE and reuses it (STABLE); clock_timestamp() reads the seam on every
                    // call (VOLATILE). Both return the seam's micros directly as timestamptz.
                    ScalarFunc::Now => {
                        let mut r = env.rng.get();
                        let micros = r.statement_clock_micros(&env.exec.seam);
                        env.rng.set(r);
                        Ok(Value::Timestamptz(micros))
                    }
                    ScalarFunc::ClockTimestamp => {
                        let r = env.rng.get();
                        let micros = r.clock_now_micros(&env.exec.seam);
                        Ok(Value::Timestamptz(micros))
                    }
                    // Sequence value functions (spec/design/sequences.md §4/§6). nextval charges an
                    // additional sequence_advance unit (the catalog-tuple read+rewrite) and mutates
                    // the per-statement pending state; currval is a pure session-state read.
                    ScalarFunc::Nextval => {
                        m.charge(COSTS.sequence_advance);
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts nextval's argument to text"),
                        };
                        Ok(Value::Int(env.exec.seq_nextval(name)?))
                    }
                    ScalarFunc::Currval => {
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts currval's argument to text"),
                        };
                        Ok(Value::Int(env.exec.seq_currval(name)?))
                    }
                    // setval charges sequence_advance (it rewrites the catalog tuple, like nextval).
                    // Arity 2 → is_called defaults true; arity 3 → the boolean third argument.
                    ScalarFunc::Setval => {
                        m.charge(COSTS.sequence_advance);
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts setval's first argument to text"),
                        };
                        let n = match &vals[1] {
                            Value::Int(n) => *n,
                            _ => unreachable!("resolver restricts setval's value to integer"),
                        };
                        let is_called = match vals.get(2) {
                            None => true,
                            Some(Value::Bool(b)) => *b,
                            Some(_) => {
                                unreachable!(
                                    "resolver restricts setval's third argument to boolean"
                                )
                            }
                        };
                        Ok(Value::Int(env.exec.seq_setval(name, n, is_called)?))
                    }
                    ScalarFunc::Lastval => Ok(Value::Int(env.exec.seq_lastval()?)),
                }
            }
            // A polymorphic array function (spec/design/array-functions.md §3). One operator_eval
            // per call; arguments charge their own. NULL handling is per-kernel (the introspectors
            // propagate, the builders are non-strict), so — unlike `ScalarFunc` — there is no
            // blanket NULL short-circuit here.
            RExpr::ArrayFunc { func, args } => {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                eval_array_func(func, &vals)
            }
            // A VARIADIC argument-counting call (spec/design/array-functions.md §12). One
            // operator_eval (the per-element/arg count walk is unmetered, like the array
            // introspectors §3.3); arguments charge their own evaluation. Non-strict — no blanket
            // NULL short-circuit. The two forms differ: the spread form counts the args' null-ness
            // (never NULL); the VARIADIC-array form returns NULL on a NULL whole-array, else counts
            // the array's flattened elements' null-ness.
            RExpr::Variadic {
                func,
                args,
                array_form,
            } => {
                m.charge(COSTS.operator_eval);
                let want_nulls = matches!(func, VariadicFunc::NumNulls);
                let count = if *array_form {
                    match args[0].eval(row, env, m)? {
                        Value::Null => return Ok(Value::Null),
                        Value::Array(a) => count_nulls(a.elements.iter(), want_nulls),
                        _ => unreachable!("resolver restricts a VARIADIC operand to an array"),
                    }
                } else {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(a.eval(row, env, m)?);
                    }
                    count_nulls(vals.iter(), want_nulls)
                };
                Ok(Value::Int(count as i64))
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
                let r = env
                    .exec
                    .exec_query_plan(plan, &child, env.params, env.ctes)?;
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
                        in_membership(&lv, &list, *negated, m)
                    }
                    // A correlated quantified subquery (array-functions.md §11.6): gather the body's
                    // single column into an array and run the SAME 3VL fold as the array form.
                    SubqueryKind::Quantified { op, all } => {
                        let lv = lhs
                            .as_ref()
                            .expect("a quantified subquery carries its resolved lhs")
                            .eval(row, env, m)?;
                        let elements: Vec<Value> = r
                            .rows
                            .into_iter()
                            .map(|mut row| row.swap_remove(0))
                            .collect();
                        let arr = if elements.is_empty() {
                            ArrayVal::empty()
                        } else {
                            ArrayVal {
                                dims: vec![elements.len()],
                                lbounds: vec![1],
                                elements,
                            }
                        };
                        quantified_membership(*op, *all, &lv, &Value::Array(arr), m)
                    }
                }
            }
            // A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
            RExpr::InValues { lhs, list, negated } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                in_membership(&lv, list, *negated, m)
            }
            // A quantified array comparison `lhs op ANY/ALL(array)` (array-functions.md §11) — the
            // array spelling of IN, the 3VL fold over the array's flattened elements.
            RExpr::Quantified {
                op,
                all,
                lhs,
                array,
            } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                let av = array.eval(row, env, m)?;
                quantified_membership(*op, *all, &lv, &av, m)
            }
        }
    }
}

/// The three-valued membership fold for `lhs op ANY/ALL(array)` (array-functions.md §11), the
/// generalization of `in_membership` to all five comparison operators and both quantifiers. A NULL
/// array → NULL; otherwise, over the flattened elements, `ANY`/`SOME` (all=false) is the OR-fold
/// (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty → FALSE) and `ALL`
/// (all=true) is the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE; empty →
/// TRUE). Each element comparison charges one `operator_eval` (+ size-scaled `decimal_work`),
/// exactly like `in_membership`, so `max_cost` bounds the walk (54P01, CLAUDE.md §13).
fn quantified_membership(
    op: CmpOp,
    all: bool,
    lv: &Value,
    av: &Value,
    m: &mut Meter,
) -> Result<Value> {
    let arr = match av {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("the resolver requires an array right operand"),
    };
    let mut any_null = false;
    for e in &arr.elements {
        m.charge(COSTS.operator_eval);
        m.charge(COSTS.decimal_work * ((decimal_cmp_work(lv, e) - 1) as i64));
        m.guard()?;
        match quantified_cmp3(op, lv, e) {
            ThreeValued::True => {
                // ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
                if !all {
                    return Ok(Value::Bool(true));
                }
            }
            ThreeValued::False => {
                // ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
                if all {
                    return Ok(Value::Bool(false));
                }
            }
            ThreeValued::Unknown => any_null = true,
        }
    }
    // Drained without a short-circuit: a NULL seen → UNKNOWN; else the quantifier's identity
    // (ALL → TRUE, ANY → FALSE — also the empty-array result).
    Ok(if any_null {
        Value::Null
    } else {
        Value::Bool(all)
    })
}

/// The per-element three-valued comparison `lhs op e` for a quantified node, normalizing a
/// mixed-width float pair to `f64` first (the resolver admits `f32` vs `f64`, matching
/// `RExpr::Compare`'s promote — here the array elements are runtime values, so the widen happens per
/// element). Bottoms out in the value module's `eq3`/`lt3`/`gt3` kernels.
///
/// A **composite** operand pair routes through the composite **total order** (`value_cmp`), NOT the
/// bare-`ROW` 3VL `eq3`/`lt3`/`gt3` (array-functions.md §13): PostgreSQL's `= ANY(addr[])` dispatches
/// on the composite `=` *operator* = `record_eq`, which is **definite with NULL fields comparable**
/// (`ROW('a',NULL)::addr = ANY(ARRAY[ROW('a',NULL)::addr])` is TRUE), the same total order
/// `array_eq` / `@>` already use for composite elements (array.md §5). A **whole-element NULL** is
/// still UNKNOWN — the operator stays strict at the value level — so the resolver-guaranteed
/// same-type pair is composite-vs-composite or composite-vs-NULL.
fn quantified_cmp3(op: CmpOp, x: &Value, e: &Value) -> ThreeValued {
    if matches!(x, Value::Composite(_)) || matches!(e, Value::Composite(_)) {
        // A whole-element NULL → UNKNOWN (3VL at the value level); else the definite total order.
        if matches!(x, Value::Null) || matches!(e, Value::Null) {
            return ThreeValued::Unknown;
        }
        let ord = value_cmp(x, e);
        let matched = match op {
            CmpOp::Eq => ord == std::cmp::Ordering::Equal,
            CmpOp::Ne => ord != std::cmp::Ordering::Equal,
            CmpOp::Lt => ord == std::cmp::Ordering::Less,
            CmpOp::Gt => ord == std::cmp::Ordering::Greater,
            CmpOp::Le => ord != std::cmp::Ordering::Greater,
            CmpOp::Ge => ord != std::cmp::Ordering::Less,
        };
        return if matched {
            ThreeValued::True
        } else {
            ThreeValued::False
        };
    }
    let (xw, ew);
    let (a, b): (&Value, &Value) = match (x, e) {
        (Value::Float32(v), Value::Float64(_)) => {
            xw = Value::Float64(*v as f64);
            (&xw, e)
        }
        (Value::Float64(_), Value::Float32(v)) => {
            ew = Value::Float64(*v as f64);
            (x, &ew)
        }
        _ => (x, e),
    };
    match op {
        CmpOp::Eq => a.eq3(b),
        CmpOp::Ne => a.eq3(b).not(),
        CmpOp::Lt => a.lt3(b),
        CmpOp::Gt => a.gt3(b),
        CmpOp::Le => a.lt3(b).or(a.eq3(b)),
        CmpOp::Ge => a.gt3(b).or(a.eq3(b)),
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
/// result type (the i16+i16 → i16 boundary — spec/design/functions.md §7).
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
            // `x % -1` is mathematically 0 for every x. Special-cased so i64::MIN % -1
            // returns 0 instead of trapping on the i64 IDIV overflow (which `checked_rem`
            // reports as None) — matching PostgreSQL and the i16/i32 widths, which
            // already compute 0 cleanly in 64-bit (spec/design/types.md §3).
            if y == -1 { Some(0) } else { x.checked_rem(y) }
        }
    };
    let v = computed.ok_or_else(|| overflow(result))?;
    if result.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(result))
    }
}

/// Evaluate `f64 ⊕ f64` for one node (spec/design/float.md §5): the IEEE correctly-rounded
/// op (round-ties-to-even — Rust's default). The PG TRAP model: a FINITE pair whose result
/// overflows to ±Inf traps 22003 (finite arithmetic never PRODUCES non-finite values); `x / 0`
/// (or `x % 0`) traps 22012 for EVERY numerator except NaN (`Inf/0` and `0/0` trap; only `NaN/0`
/// propagates to NaN — matching PG). An operand already Inf/NaN otherwise PROPAGATES (no trap).
fn eval_float64_arith(op: ArithOp, x: f64, y: f64) -> Result<Value> {
    // Division/modulus by a zero divisor traps 22012 for every numerator EXCEPT NaN, which
    // propagates (NaN/0 = NaN, matching PG). `Inf/0` and `0/0` are genuine division by zero.
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y, // IEEE fmod (Rust `%` on f64 is fmod)
    };
    // Finite-overflow trap (§3): a result that became ±Inf from two FINITE operands overflowed.
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float64));
    }
    Ok(Value::Float64(r))
}

/// As [`eval_float64_arith`], at binary32 (`f32`). Each op rounds to binary32 (native `f32`
/// arithmetic), so a finite overflow to ±Inf at the f32 range traps 22003.
fn eval_float32_arith(op: ArithOp, x: f32, y: f32) -> Result<Value> {
    // Same zero-divisor rule as f64: traps for every numerator except NaN (Inf/0 traps).
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y,
    };
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float32));
    }
    Ok(Value::Float32(r))
}

/// Cast an integer to a float of `target` width (spec/design/float.md §6): nearest, round-ties-to-
/// even (Rust `as`), never overflows (i64::MAX < f32::MAX). `target` is a float type.
fn int_to_float(n: i64, target: ScalarType) -> Value {
    if target.is_float32() {
        Value::Float32(n as f32)
    } else {
        Value::Float64(n as f64)
    }
}

/// An integer literal adapted to a float context as a constant `RExpr` (spec/design/float.md §4),
/// at the context width.
fn int_to_const_float(n: i64, target: ScalarType) -> RExpr {
    if target.is_float32() {
        RExpr::ConstFloat32(n as f32)
    } else {
        RExpr::ConstFloat64(n as f64)
    }
}

/// Cast a decimal to a float of `target` width (spec/design/float.md §6): the nearest binary value
/// to the decimal's exact value. A magnitude that overflows the float range traps 22003 (not ±Inf
/// — the §3 finite rule; decimal is always finite, so the result can only be finite or trap).
fn decimal_to_float(d: &Decimal, target: ScalarType) -> Result<Value> {
    // The decimal's canonical string parses to the nearest binary value (Rust's float parser is
    // correctly rounded). A huge decimal parses to ±Inf, which is the overflow case.
    let s = d.render();
    if target.is_float32() {
        let f: f32 = s.parse().map_err(|_| overflow(ScalarType::Float32))?;
        if f.is_finite() {
            Ok(Value::Float32(f))
        } else {
            Err(overflow(ScalarType::Float32))
        }
    } else {
        let f: f64 = s.parse().map_err(|_| overflow(ScalarType::Float64))?;
        if f.is_finite() {
            Ok(Value::Float64(f))
        } else {
            Err(overflow(ScalarType::Float64))
        }
    }
}

/// Cast a float value (already widened to f64) to a non-float `target` — int / decimal — or to a
/// narrower float width (spec/design/float.md §6). NaN/±Inf → 22003 for every non-float target
/// (and for f64 → f32 overflow). Float → int rounds HALF AWAY FROM ZERO (jed's one mode)
/// then range-checks. Float → decimal is the exact decimal of the binary value, then the typmod.
fn cast_from_float(f: f64, target: ScalarType, typmod: Option<DecimalTypmod>) -> Result<Value> {
    if target.is_float64() {
        // float → f64: widening (lossless from f32, identity from f64).
        return Ok(Value::Float64(f));
    }
    if target.is_float32() {
        // f64 → f32: nearest (round-ties-to-even). A finite value beyond the binary32
        // range traps 22003 (not ±Inf); NaN/±Inf convert across widths unchanged (propagate).
        let n = f as f32;
        if n.is_infinite() && f.is_finite() {
            return Err(overflow(ScalarType::Float32));
        }
        return Ok(Value::Float32(n));
    }
    // Non-float targets reject NaN/±Inf (they have no finite representation).
    if !f.is_finite() {
        return Err(overflow(target));
    }
    if target.is_decimal() {
        // float → decimal: the EXACT decimal of the binary value (spec/design/float.md §6), then
        // the typmod's scale coercion. `f` is finite (checked above); a f32 reaches here
        // already losslessly widened to f64, so the exact decimal IS the binary32 value's.
        let d = Decimal::from_float64(f);
        return Ok(Value::Decimal(coerce_decimal(d, typmod)?));
    }
    // float → int: round HALF AWAY FROM ZERO, then range-check the target integer (22003).
    let rounded = f.round(); // Rust `f64::round` is round-half-away-from-zero
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return Err(overflow(target));
    }
    let v = rounded as i64;
    if target.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(target))
    }
}

/// Finalize a float SUM/AVG as the order-independent CANONICAL-ORDER FOLD (spec/design/float.md §7),
/// bit-identical across cores and across any serial/parallel plan. The steps, in order:
/// 1. Special values FIRST (order-independent): empty/all-NULL group → NULL (an aggregate over no
///    rows); any NaN → NaN; both +Inf and -Inf → NaN; else +Inf → +Inf; else -Inf → -Inf; else
///    all-finite → step 2.
/// 2. Sort the (already `-0`-canonicalized) finite inputs by the §3 total order — distinct values
///    have distinct keys, so the sort is total/deterministic.
/// 3. Fold left with width-correct IEEE add (a running total overflowing to ±Inf → 22003).
///
/// AVG = SUM / count, ONE final rounding at the input width.
#[allow(clippy::too_many_arguments)]
fn finalize_float_fold(
    width: ScalarType,
    is_avg: bool,
    mut finite: Vec<f64>,
    count: i64,
    any_nan: bool,
    pos_inf: bool,
    neg_inf: bool,
) -> Result<Value> {
    let is_f32 = width.is_float32();
    let wrap = |f: f64| -> Value {
        if is_f32 {
            Value::Float32(f as f32)
        } else {
            Value::Float64(f)
        }
    };
    // Step 1 — empty group → NULL (no non-NULL inputs).
    if count == 0 {
        return Ok(Value::Null);
    }
    // Step 1 — special values, resolved before any finite sum (order-independent).
    if any_nan {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf && neg_inf {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf {
        return Ok(wrap(f64::INFINITY));
    }
    if neg_inf {
        return Ok(wrap(f64::NEG_INFINITY));
    }
    // Step 2 — sort the finite inputs by the total order (all finite, so a plain partial_cmp is
    // total here; -0 already canonicalized to +0 at fold time).
    finite.sort_by(|a, b| crate::value::total_cmp_f64(*a, *b));
    // Step 3 — fold left at the input width (round each add to the width). A running total that
    // overflows to ±Inf from finite operands traps 22003 (the §3 finite-overflow rule).
    let sum = if is_f32 {
        let mut acc: f32 = 0.0;
        for &v in &finite {
            acc += v as f32;
            if acc.is_infinite() {
                return Err(overflow(ScalarType::Float32));
            }
        }
        acc as f64
    } else {
        let mut acc: f64 = 0.0;
        for &v in &finite {
            acc += v;
            if acc.is_infinite() {
                return Err(overflow(ScalarType::Float64));
            }
        }
        acc
    };
    if !is_avg {
        return Ok(wrap(sum));
    }
    // AVG = SUM / count, one rounding at the input width.
    if is_f32 {
        let avg = (sum as f32) / (count as f32);
        Ok(Value::Float32(avg))
    } else {
        Ok(Value::Float64(sum / count as f64))
    }
}

/// `round(f64, places)` — round half away from zero to `places` decimal digits (the engine's
/// one mode — spec/design/float.md §8). A NaN/±Inf operand passes through. `places` may be
/// negative (round to the left of the point). Done by scaling by 10^places, `round()` (half-away),
/// then unscaling — the approximate float path (binary, so itself inexact, which the `R` tag
/// absorbs).
fn round_f64_places(f: f64, places: i64) -> f64 {
    if !f.is_finite() {
        return f;
    }
    if places == 0 {
        return f.round();
    }
    let scale = 10f64.powi(places as i32);
    if !scale.is_finite() || scale == 0.0 {
        // Extreme `places` — clamp to the operand (no observable rounding at that magnitude).
        return f;
    }
    (f * scale).round() / scale
}

/// Evaluate a float scalar function over a finite/non-finite f64 (spec/design/float.md §8). The
/// EXACT set (ceil/floor/trunc/sqrt) is correctly-rounded (in-contract); the TRANSCENDENTAL set
/// (exp/ln/log10/pow/sin/cos/tan) calls Rust's libm (exempted, may differ by an ULP cross-core).
/// Domain/overflow errors trap 22003, keeping NaN/Inf input-only (a NaN/Inf *operand* propagates).
fn eval_float_func(func: ScalarFunc, x: f64, arg2: Option<&Value>) -> Result<Value> {
    let r = match func {
        ScalarFunc::Ceil => x.ceil(),
        ScalarFunc::Floor => x.floor(),
        ScalarFunc::Trunc => x.trunc(),
        ScalarFunc::Sqrt => {
            // sqrt of a NEGATIVE finite value is a domain error → 22003 (NaN stays input-only).
            // A NaN/±Inf operand propagates (sqrt(Inf) = Inf, sqrt(NaN) = NaN).
            if x.is_finite() && x < 0.0 {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "cannot take square root of a negative number",
                ));
            }
            x.sqrt()
        }
        ScalarFunc::Exp => {
            let v = x.exp();
            // exp overflow (e.g. exp(710)) → ±Inf from a finite operand traps 22003.
            if v.is_infinite() && x.is_finite() {
                return Err(overflow(ScalarType::Float64));
            }
            v
        }
        ScalarFunc::Ln => {
            // ln(0) → 22003; ln(neg) → 22003 (domain). NaN/Inf operands propagate.
            if x.is_finite() {
                if x == 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of zero",
                    ));
                }
                if x < 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of a negative number",
                    ));
                }
            }
            x.ln()
        }
        ScalarFunc::Log10 => {
            if x.is_finite() {
                if x == 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of zero",
                    ));
                }
                if x < 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of a negative number",
                    ));
                }
            }
            x.log10()
        }
        ScalarFunc::Pow => {
            let y = match arg2 {
                Some(Value::Float64(y)) => *y,
                _ => unreachable!("pow's second arg is a widened f64"),
            };
            let v = x.powf(y);
            // pow overflow from finite operands → 22003.
            if v.is_infinite() && x.is_finite() && y.is_finite() {
                return Err(overflow(ScalarType::Float64));
            }
            v
        }
        ScalarFunc::Sin => x.sin(),
        ScalarFunc::Cos => x.cos(),
        ScalarFunc::Tan => x.tan(),
        ScalarFunc::Abs
        | ScalarFunc::Round
        | ScalarFunc::MakeInterval
        | ScalarFunc::UuidExtractVersion
        | ScalarFunc::UuidExtractTimestamp
        | ScalarFunc::Uuidv4
        | ScalarFunc::Uuidv7
        | ScalarFunc::Now
        | ScalarFunc::ClockTimestamp
        | ScalarFunc::Nextval
        | ScalarFunc::Currval
        | ScalarFunc::Setval
        | ScalarFunc::Lastval => {
            unreachable!(
                "abs/round/make_interval/uuid_*/now/clock_timestamp/sequence fns are handled before eval_float_func"
            )
        }
    };
    Ok(Value::Float64(r))
}

/// Widen a numeric value to `Decimal` (an integer operand of decimal arithmetic).
fn to_decimal(v: Value) -> Decimal {
    match v {
        Value::Decimal(d) => d,
        Value::Int(n) => Decimal::from_i64(n),
        _ => unreachable!("resolver guarantees a numeric operand here"),
    }
}

/// The `decimal_work` W of an arithmetic node — which group-count formula applies per op
/// (spec/design/cost.md §3 "decimal_work"). The evaluator charges W − 1 before the op runs.
fn decimal_arith_work(op: ArithOp, a: &Decimal, b: &Decimal) -> u64 {
    match op {
        ArithOp::Add | ArithOp::Sub => decimal::work_linear(a, b),
        ArithOp::Mul => decimal::work_mul(a, b),
        ArithOp::Div => decimal::work_div(a, b),
        ArithOp::Mod => decimal::work_mod(a, b),
    }
}

/// The `decimal_work` W of a comparison over a decimal(-promotable) pair — the aligned
/// linear formula after `int → decimal` promotion; 1 (no charge) for any other pair,
/// including a NULL side, where no decimal compare runs (cost.md §3 "decimal_work").
fn decimal_cmp_work(a: &Value, b: &Value) -> u64 {
    match (a, b) {
        (Value::Decimal(x), Value::Decimal(y)) => decimal::work_linear(x, y),
        (Value::Decimal(x), Value::Int(y)) => decimal::work_linear(x, &Decimal::from_i64(*y)),
        (Value::Int(x), Value::Decimal(y)) => decimal::work_linear(&Decimal::from_i64(*x), y),
        _ => 1,
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
pub(crate) fn key_cmp(
    a: &Value,
    b: &Value,
    descending: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
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
        // Floats order by the PG total order (NaN largest, -0 = +0; spec/design/float.md §3).
        (Value::Float32(x), Value::Float32(y)) => crate::value::total_cmp_f32(*x, *y),
        (Value::Float64(x), Value::Float64(y)) => crate::value::total_cmp_f64(*x, *y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        // Timestamps order by the i64 instant (-infinity < finite < infinity).
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        // Intervals order by the canonical 128-bit span (spec/design/interval.md §2).
        (Value::Interval(x), Value::Interval(y)) => x.cmp(y),
        // A composite sorts lexicographically, NULLs-last per field (the composite sort key —
        // spec/design/composite.md §5): the first non-equal field decides, recursing through
        // `key_cmp` so per-field NULL placement and nested composites are handled uniformly. The
        // caller's `descending` flip in `key_cmp` reverses the whole tuple. A row-size tie-break
        // keeps it total (same-type rows have equal arity, so it is only reached for safety).
        (Value::Composite(x), Value::Composite(y)) => {
            for (xf, yf) in x.iter().zip(y.iter()) {
                let c = key_cmp(xf, yf, false, false);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        // An array sorts by the PG `array_cmp` total order (spec/design/array.md §5): element-wise
        // over the flattened elements (NULLs-last per element, recursing through `key_cmp`), then
        // fewer elements first, then smaller ndim, then per dimension (length, then lower bound).
        (Value::Array(x), Value::Array(y)) => {
            for (xe, ye) in x.elements.iter().zip(y.elements.iter()) {
                let c = key_cmp(xe, ye, false, false);
                if c != Ordering::Equal {
                    return c;
                }
            }
            let mut c = x.elements.len().cmp(&y.elements.len());
            if c != Ordering::Equal {
                return c;
            }
            c = x.dims.len().cmp(&y.dims.len());
            if c != Ordering::Equal {
                return c;
            }
            for d in 0..x.dims.len() {
                c = x.dims[d]
                    .cmp(&y.dims[d])
                    .then(x.lbounds[d].cmp(&y.lbounds[d]));
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
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
        Value::Interval(_) => 8,
        Value::Float32(_) => 9,
        Value::Float64(_) => 10,
        Value::Date(_) => 13,
        // A composite sorts only against composites of its own type (ORDER BY is single-typed), so
        // this cross-family rank is only for totality; it sits after the scalar families.
        Value::Composite(_) => 11,
        // An array sorts only against arrays of its own element type (ORDER BY is single-typed), so
        // this cross-family rank is only for totality; it sits after composite.
        Value::Array(_) => 12,
        // Poisoned (large-values.md §14): ORDER BY slots are in the touched set, so a sort
        // key is always resolved before it reaches the comparator.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    // The function registry (extensibility.md §5) is data-driven over the generated catalog
    // tables, but two halves stay hand-written per core: the scalar kernel id (`scalar_func_id`)
    // and the result-code / plan interpreters. This guards against drift — a catalog row added
    // without a matching kernel id or with a result code no interpreter handles fails here, not
    // silently at some query's resolve.
    #[test]
    fn registry_covers_catalog() {
        for o in OPERATORS.iter().filter(|o| o.kind == "function") {
            if is_array_func_name(o.name) {
                // A polymorphic array function (array-functions.md §2): its kernel id comes from
                // `array_func_id` and its result is a reserved poly code or a scalar id.
                let _ = array_func_id(o.name);
                let concrete_array = o
                    .result
                    .strip_suffix("[]")
                    .is_some_and(|base| ScalarType::from_name(base).is_some());
                assert!(
                    o.result == "anyarray"
                        || o.result == "anyelement"
                        || concrete_array
                        || ScalarType::from_name(o.result).is_some(),
                    "array function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            if is_variadic_func_name(o.name) {
                // A VARIADIC function (array-functions.md §12): its kernel id comes from
                // `variadic_func_id` and its result is a concrete scalar id.
                let _ = variadic_func_id(o.name);
                assert!(
                    ScalarType::from_name(o.result).is_some(),
                    "variadic function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            // Every function name maps to a kernel id (panics via unreachable! if not).
            let _ = scalar_func_id(o.name);
            // Every function result code is one the interpreter understands: "promoted" or a
            // literal scalar-type id.
            assert!(
                o.result == "promoted" || ScalarType::from_name(o.result).is_some(),
                "function {} has unhandled result code {}",
                o.name,
                o.result
            );
        }
        for a in AGGREGATES.iter() {
            assert!(
                matches!(a.result, "i64" | "decimal" | "sum_widen" | "same_as_input"),
                "aggregate {} has unhandled result code {}",
                a.name,
                a.result
            );
            // Every overload is reachable: a star row via `aggregate_has_star`, an expr row via
            // `lookup_aggregate_overload` over a representative operand of its declared family.
            if a.arg == "star" {
                assert!(aggregate_has_star(a.surface), "{} star overload", a.surface);
            } else {
                let probe = match a.arg_families.first().copied() {
                    Some("integer") => ResolvedType::Int(ScalarType::Int32),
                    Some("decimal") => ResolvedType::Decimal,
                    Some("float") => ResolvedType::Float(ScalarType::Float64),
                    _ => ResolvedType::Int(ScalarType::Int32), // "any"
                };
                let found = lookup_aggregate_overload(a.surface, &probe)
                    .expect("expr overload resolves for its declared family");
                // And its plan/result selection is total (panics via unreachable! otherwise).
                let lname = a.surface.to_ascii_lowercase();
                let _ = aggregate_plan(&lname, found.result, &probe);
            }
        }
    }
}
