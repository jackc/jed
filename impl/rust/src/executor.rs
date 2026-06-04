//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{
    BinaryOp, CreateTable, Delete, DropTable, Expr, Insert, JoinKind, Literal, Select, SelectItems,
    Statement, TypeMod, UnaryOp, Update,
};
use crate::catalog::{Column, Table};
use crate::cost::Meter;
use crate::costs::COSTS;
use crate::decimal::{Decimal, MAX_PRECISION, MAX_SCALE};
use crate::encoding::encode_int;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::{Row, TableStore};
use crate::types::{DecimalTypmod, ScalarType};
use crate::value::{Value, and3, from3, not3, or3, parse_bytea_hex};
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

/// The whole database: catalog + per-table in-memory stores. Single committed
/// state (CLAUDE.md §3); the staging-buffer commit model lands with persistence.
#[derive(Default)]
pub struct Database {
    tables: HashMap<String, Table>,
    stores: HashMap<String, TableStore>,
}

impl Database {
    pub fn new() -> Self {
        Database {
            tables: HashMap::new(),
            stores: HashMap::new(),
        }
    }

    /// Look up a table definition by name (case-insensitive).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the
    /// table does not exist. Used by SELECT and by tests.
    pub fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.stores
            .get(&name.to_ascii_lowercase())
            .map(|s| s.iter_in_key_order().cloned().collect())
    }

    /// Register a new table and its (empty) store. Lower-cased name is the key.
    pub(crate) fn put_table(&mut self, table: Table) {
        let key = table.name.to_ascii_lowercase();
        self.stores.insert(key.clone(), TableStore::new());
        self.tables.insert(key, table);
    }

    /// Every table with its store, as `(lowercased key, table, store)` tuples, for
    /// the on-disk serializer (spec/fileformat/format.md). The serializer sorts by
    /// the lowercased key so hash-map iteration order never leaks (CLAUDE.md §8).
    pub(crate) fn catalog_and_stores(&self) -> Vec<(&str, &Table, &TableStore)> {
        self.tables
            .iter()
            .map(|(k, t)| (k.as_str(), t, self.stores.get(k).expect("store exists")))
            .collect()
    }

    /// Execute one parsed statement.
    pub fn execute_stmt(&mut self, stmt: Statement) -> Result<Outcome> {
        match stmt {
            Statement::CreateTable(ct) => self.execute_create_table(ct),
            Statement::DropTable(dt) => self.execute_drop_table(dt),
            Statement::Insert(ins) => self.execute_insert(ins),
            Statement::Select(sel) => self.execute_select(sel),
            Statement::Update(upd) => self.execute_update(upd),
            Statement::Delete(del) => self.execute_delete(del),
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
                // Only integers may be a key this slice. The order-preserving text and decimal
                // key encodings (spec/design/encoding.md §2.4/§2.5) are authored but
                // unexercised, so a text or decimal PRIMARY KEY is a documented 0A000 narrowing
                // (spec/design/types.md §11/§12).
                if !ty.is_integer() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                // Likewise boolean: the bool-byte key encoding rule is authored but
                // unexercised, so a boolean PRIMARY KEY is a documented 0A000 narrowing
                // (spec/design/types.md §9), relaxable in a later boolean-in-key slice.
                if ty.is_bool() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a boolean primary key is not supported yet",
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
            columns.push(Column {
                name: def.name.clone(),
                ty,
                decimal,
                primary_key: def.primary_key,
                not_null: def.primary_key, // PRIMARY KEY ⇒ NOT NULL
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
        self.tables.remove(&key);
        self.stores.remove(&key);
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Analyze and run an INSERT of one or more rows. Each row maps its literal values
    /// positionally to columns and is type-checked (NULL into NOT NULL traps 23502; an
    /// integer outside the column type's range traps 22003 — CLAUDE.md §8); a duplicate
    /// primary key traps 23505. A multi-row INSERT is **two-phase / all-or-nothing**
    /// (spec/design/grammar.md §12), mirroring UPDATE: every row is validated — including
    /// its storage key checked against both the stored rows and earlier rows in the same
    /// statement — before any row is inserted, so a mid-batch failure stores nothing.
    fn execute_insert(&mut self, ins: Insert) -> Result<Outcome> {
        let table = self.table(&ins.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", ins.table),
            )
        })?;

        // Snapshot the catalog data each row is validated against, ending the `table`
        // borrow so phase 1 can read the store (dup-key check) and phase 2 can mutate it.
        let table_name = table.name.clone();
        let columns: Vec<Column> = table.columns.clone();
        let pk = table.primary_key_index().map(|i| (i, table.columns[i].ty));

        // Phase 1 — validate every row and compute its key. Nothing is stored yet. For a
        // table with a primary key, `key` is Some(encoded) and is checked for a duplicate
        // (within the batch via `seen_keys`, and against the store) up front; for a table
        // with none it is None and a fresh monotonic rowid is allocated in phase 2.
        let mut prepared: Vec<(Option<Vec<u8>>, Row)> = Vec::with_capacity(ins.rows.len());
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
        for lits in &ins.rows {
            if lits.len() != columns.len() {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    format!(
                        "INSERT row has {} values but table {} has {} columns",
                        lits.len(),
                        table_name,
                        columns.len()
                    ),
                ));
            }

            let mut row = Vec::with_capacity(columns.len());
            for (col, lit) in columns.iter().zip(lits) {
                // The literal adapts/coerces to its target column: an integer literal into a
                // decimal column widens (int→decimal, then to the typmod); a decimal literal
                // into a decimal column rounds to its scale; a cross-family pair is 42804
                // (spec/design/decimal.md §6, types.md §5).
                let raw = literal_to_value(lit);
                let value = store_value(raw, col.ty, col.decimal, col.not_null, &col.name)?;
                row.push(value);
            }

            let key = match pk {
                Some((i, pk_ty)) => {
                    let k = match &row[i] {
                        Value::Int(n) => encode_int(pk_ty, *n),
                        // Unreachable: a PK column is NOT NULL, enforced above.
                        Value::Null => unreachable!("primary key column is NOT NULL"),
                        // Unreachable: a boolean PRIMARY KEY is rejected at CREATE TABLE (0A000).
                        Value::Bool(_) => {
                            unreachable!("a boolean primary key is rejected at CREATE TABLE")
                        }
                        // Unreachable: a text/decimal/bytea PRIMARY KEY is rejected at CREATE
                        // TABLE (0A000) — non-integer PKs are caught by `!is_integer()`.
                        Value::Text(_) | Value::Decimal(_) | Value::Bytea(_) => {
                            unreachable!("a non-integer primary key is rejected at CREATE TABLE")
                        }
                    };
                    if seen_keys.contains(&k) || self.store(&ins.table).get(&k).is_some() {
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

        // Phase 2 — every row validated, so each insert is guaranteed to succeed. A
        // synthetic rowid is allocated here, in row order, so a failed validation pass
        // burns none (spec/fileformat/format.md, spec/design/grammar.md §12).
        let store = self.store_mut(&ins.table);
        for (key, row) in prepared {
            let key = key.unwrap_or_else(|| encode_int(ScalarType::Int64, store.alloc_rowid()));
            assert!(
                store.insert(key, row),
                "pre-validated INSERT key must be unique"
            );
        }
        // INSERT of literal rows reads no rows and evaluates no expression tree: zero
        // cost (DEFAULT expressions, when added, will accrue here).
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Analyze and run a DELETE: resolve the table and optional predicate, collect
    /// the keys of matching rows (only a TRUE predicate matches — Kleene), then
    /// remove them. No WHERE deletes every row. Keys are collected before mutating
    /// so the map is not modified while iterating.
    fn execute_delete(&mut self, del: Delete) -> Result<Outcome> {
        let table = self.table(&del.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", del.table),
            )
        })?;
        // DELETE is single-table; resolve its WHERE against a one-relation scope.
        let scope = Scope::single(table);
        let filter = match &del.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p)?),
            None => None,
        };

        // Collect matching keys before mutating (so the map is not modified mid-scan).
        // A WHERE arithmetic can trap (22003/22012), so this is an explicit loop that
        // propagates the error rather than a `.filter` closure. Each scanned row and each
        // filter evaluation accrues cost (CLAUDE.md §13; spec/design/cost.md §3).
        let mut meter = Meter::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for (k, row) in self.store(&del.table).iter_entries() {
            meter.charge(COSTS.storage_row_read);
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(row, &mut meter)?.is_true(),
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
    fn execute_update(&mut self, upd: Update) -> Result<Outcome> {
        let table = self.table(&upd.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", upd.table),
            )
        })?;
        // UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
        // shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
        let scope = Scope::single(table);

        // Resolve assignments up front (fail fast, deterministic).
        let pk_idx = table.primary_key_index();
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
            let (source, ty) = resolve(&scope, &a.value, Some(col.ty))?;
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

        let filter = match &upd.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p)?),
            None => None,
        };

        // Phase 1: build + validate every matching row's new values; no writes yet. Each
        // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes
        // do not — they evaluate nothing; spec/design/cost.md §3).
        let mut meter = Meter::new();
        let mut updates: Vec<(Vec<u8>, Row)> = Vec::new();
        for (key, row) in self.store(&upd.table).iter_entries() {
            meter.charge(COSTS.storage_row_read);
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(row, &mut meter)?.is_true(),
            };
            if !matched {
                continue;
            }
            let mut new_row = row.clone();
            for plan in &plans {
                let raw = plan.source.eval(row, &mut meter)?;
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

    /// Analyze and run a SELECT: resolve projected columns and the WHERE/ORDER BY
    /// columns against the catalog, scan the table in primary-key order, filter by
    /// the predicate (three-valued — only TRUE keeps a row), optionally re-sort by
    /// ORDER BY, then project. Rows are produced in a deterministic order
    /// (CLAUDE.md §10).
    fn execute_select(&mut self, sel: Select) -> Result<Outcome> {
        // Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
        // relation's flat column offset in FROM order, and reject a duplicate label — a
        // self-join without distinct aliases is 42712 (spec/design/grammar.md §15).
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
        let scope = Scope { rels };

        // Resolve projections (paired with output column names — grammar.md §8), the optional
        // WHERE (must be boolean), and the ORDER BY keys against the full scope. A bare key
        // ambiguous across relations is 42702; an unknown qualifier is 42P01 (§15).
        let (projections, column_names) = resolve_projections(&scope, &sel.items)?;
        let filter = match &sel.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p)?),
            None => None,
        };
        let mut order: Vec<(usize, bool, bool)> = Vec::with_capacity(sel.order_by.len());
        for key in &sel.order_by {
            let idx = match &key.qualifier {
                Some(q) => scope.resolve_qualified(q, &key.column)?,
                None => scope.resolve_bare(&key.column)?,
            };
            order.push((idx, key.descending, key.nulls_first));
        }

        // SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates are
        // collapsed, an ORDER BY key not in the projected output has no single value per row,
        // so each key must appear as a bare/qualified column in the select list (resolved to
        // the same flat index; or the list is `*`). Matches PostgreSQL (42P10). Aliases are
        // invisible to ORDER BY (§8), so an aliased bare column still counts as projecting it.
        if sel.distinct && !order.is_empty() {
            if let SelectItems::Items(items) = &sel.items {
                let mut projected: HashSet<usize> = HashSet::new();
                for it in items {
                    let idx = match &it.expr {
                        Expr::Column(name) => scope.resolve_bare(name).ok(),
                        Expr::QualifiedColumn { qualifier, name } => {
                            scope.resolve_qualified(qualifier, name).ok()
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
        // (spec/design/grammar.md §15).
        let mut join_ons: Vec<Option<RExpr>> = Vec::with_capacity(sel.joins.len());
        for (k, j) in sel.joins.iter().enumerate() {
            match &j.on {
                None => join_ons.push(None),
                Some(on_expr) => {
                    let partial = Scope {
                        rels: scope.rels[..=k + 1].to_vec(),
                    };
                    join_ons.push(Some(resolve_boolean_filter(&partial, on_expr)?));
                }
            }
        }

        // Materialize each base table once, in primary-key order, charging storage_row_read
        // per physical row (spec/design/cost.md §3 JOIN). The nested loop re-reads from these
        // in-memory buffers, which are not stores and charge nothing.
        let mut meter = Meter::new();
        let mut materialized: Vec<Vec<Row>> = Vec::with_capacity(scope.rels.len());
        for rel in &scope.rels {
            let mut table_rows: Vec<Row> = Vec::new();
            for row in self.store(&rel.table.name).iter_in_key_order() {
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
        for (k, j) in sel.joins.iter().enumerate() {
            let right_rows = &materialized[k + 1];
            let on = &join_ons[k];
            let emit_left = matches!(j.kind, JoinKind::Left | JoinKind::Full);
            let emit_right = matches!(j.kind, JoinKind::Right | JoinKind::Full);
            // NULL-pad widths come from the SCOPE, never a sampled row, so they are correct even
            // when `running`/`right_rows` is empty: the right table begins at flat offset
            // rels[k+1].offset (= the width of every running row) and is that many columns wide.
            let left_pad = scope.rels[k + 1].offset;
            let right_pad = scope.rels[k + 1].table.columns.len();
            let mut next: Vec<Row> = Vec::new();
            let mut right_matched = vec![false; right_rows.len()];
            for left in &running {
                let mut left_matched = false;
                for (ri, right) in right_rows.iter().enumerate() {
                    let mut combined = left.clone();
                    combined.extend_from_slice(right);
                    let keep = match on {
                        None => true,
                        Some(pred) => pred.eval(&combined, &mut meter)?.is_true(),
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
            let keep = match &filter {
                None => true,
                Some(f) => f.eval(&row, &mut meter)?.is_true(),
            };
            if keep {
                rows.push(row);
            }
        }

        // ORDER BY: a stable sort applying each key left to right — the first non-equal key
        // decides, and a full tie keeps the scan order (the sort is stable). Each key's NULL
        // placement is decoupled from its value-direction flip, so an explicit NULLS
        // FIRST|LAST overrides the default (spec/design/grammar.md §10).
        if !order.is_empty() {
            rows.sort_by(|a, b| {
                for &(idx, descending, nulls_first) in &order {
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
            let start = sel.offset.unwrap_or(0).min(len as i64) as usize;
            let end = match sel.limit {
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
        let out_rows = if sel.distinct {
            // Project every filtered row (charging projection cost per row, the §3
            // asymmetry), keeping first occurrences. `seen` is membership-only: the
            // output order comes from the deterministic source iteration, never from set
            // iteration (no hashmap-order leak — CLAUDE.md §8/§10).
            let mut seen: std::collections::HashSet<Vec<Value>> = std::collections::HashSet::new();
            let mut distinct_rows: Vec<Vec<Value>> = Vec::new();
            for row in &rows {
                let mut out = Vec::with_capacity(projections.len());
                for p in &projections {
                    out.push(p.eval(row, &mut meter)?);
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
                let mut out = Vec::with_capacity(projections.len());
                for p in &projections {
                    out.push(p.eval(row, &mut meter)?);
                }
                out_rows.push(out);
            }
            out_rows
        };

        Ok(Outcome::Query {
            column_names,
            rows: out_rows,
            cost: meter.accrued,
        })
    }

    /// Shared read access to a table's store (the table is known to exist).
    fn store(&self, name: &str) -> &TableStore {
        self.stores
            .get(&name.to_ascii_lowercase())
            .expect("store exists for a known table")
    }

    /// Mutable access to a table's store (the table is known to exist).
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        self.stores
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a known table")
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

/// The relations a query's FROM clause puts in scope, in FROM order.
struct Scope<'a> {
    rels: Vec<ScopeRel<'a>>,
}

impl<'a> Scope<'a> {
    /// A one-relation scope (the single-table SELECT / UPDATE / DELETE case).
    fn single(table: &'a Table) -> Scope<'a> {
        Scope {
            rels: vec![ScopeRel {
                label: table.name.to_ascii_lowercase(),
                table,
                offset: 0,
            }],
        }
    }

    /// Resolve a bare column name to a flat row index: no relation has it → 42703; two or
    /// more relations have it → 42702 ambiguous; exactly one → its flat index.
    fn resolve_bare(&self, name: &str) -> Result<usize> {
        let mut found: Option<usize> = None;
        for r in &self.rels {
            if let Some(local) = r.table.column_index(name) {
                if found.is_some() {
                    return Err(ambiguous_column(name));
                }
                found = Some(r.offset + local);
            }
        }
        found.ok_or_else(|| undefined_column(name))
    }

    /// Resolve a qualified `rel.col` to a flat row index: an unknown `rel` is 42P01, a known
    /// `rel` with no such column is 42703. Never ambiguous (it names one relation).
    fn resolve_qualified(&self, qualifier: &str, name: &str) -> Result<usize> {
        let q = qualifier.to_ascii_lowercase();
        let rel = self
            .rels
            .iter()
            .find(|r| r.label == q)
            .ok_or_else(|| missing_from_entry(qualifier))?;
        let local = rel
            .table
            .column_index(name)
            .ok_or_else(|| undefined_column(name))?;
        Ok(rel.offset + local)
    }

    /// The column at a flat index (the index is known valid — resolution produced it).
    fn column_at(&self, flat: usize) -> &Column {
        for r in &self.rels {
            let n = r.table.columns.len();
            if flat >= r.offset && flat < r.offset + n {
                return &r.table.columns[flat - r.offset];
            }
        }
        unreachable!("a resolved flat column index is always in range")
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
    Null,
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
    ConstNull,
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
}

/// Resolve `SELECT` items against the FROM scope into evaluable projections (any result type
/// is allowed in the select list, including boolean — `SELECT a = b`), each paired with its
/// output column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM
/// order, each relation's columns in catalog order (§15).
fn resolve_projections(scope: &Scope, items: &SelectItems) -> Result<(Vec<RExpr>, Vec<String>)> {
    match items {
        SelectItems::All => {
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            for rel in &scope.rels {
                for (i, c) in rel.table.columns.iter().enumerate() {
                    nodes.push(RExpr::Column(rel.offset + i));
                    names.push(c.name.clone());
                }
            }
            Ok((nodes, names))
        }
        SelectItems::Items(items) => {
            let mut nodes = Vec::with_capacity(items.len());
            let mut names = Vec::with_capacity(items.len());
            for it in items {
                let (node, _) = resolve(scope, &it.expr, None)?;
                names.push(match &it.alias {
                    Some(a) => a.clone(),
                    None => output_name(scope, &it.expr),
                });
                nodes.push(node);
            }
            Ok((nodes, names))
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
        Expr::Column(name) => match scope.resolve_bare(name) {
            Ok(idx) => scope.column_at(idx).name.clone(),
            Err(_) => name.clone(),
        },
        Expr::QualifiedColumn { qualifier, name } => match scope.resolve_qualified(qualifier, name)
        {
            Ok(idx) => scope.column_at(idx).name.clone(),
            Err(_) => name.clone(),
        },
        _ => "?column?".to_string(),
    }
}

/// Resolve a WHERE / ON expression: it must resolve to boolean (or an untyped NULL, which
/// is always unknown → no rows). An integer-valued WHERE/ON is a 42804 type error.
fn resolve_boolean_filter(scope: &Scope, e: &Expr) -> Result<RExpr> {
    let (node, ty) = resolve(scope, e, None)?;
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(node),
        ResolvedType::Int(_) | ResolvedType::Text | ResolvedType::Decimal | ResolvedType::Bytea => {
            Err(type_error("argument of WHERE must be boolean"))
        }
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
    } else {
        ResolvedType::Int(ty)
    }
}

/// Resolve one `Expr` into an `RExpr` plus its static type, against the FROM `scope`. `ctx`
/// is the type an untyped integer literal should adapt to (spec/design/types.md §6); `None`
/// defaults a bare literal to int64. A column reference resolves to a flat row index via the
/// scope — a bare name ambiguous across relations is 42702, an unknown qualifier is 42P01
/// (spec/design/grammar.md §15).
fn resolve(scope: &Scope, e: &Expr, ctx: Option<ScalarType>) -> Result<(RExpr, ResolvedType)> {
    match e {
        Expr::Column(name) => {
            let idx = scope.resolve_bare(name)?;
            let ty = scope.column_at(idx).ty;
            Ok((RExpr::Column(idx), resolved_type_of(ty)))
        }
        Expr::QualifiedColumn { qualifier, name } => {
            let idx = scope.resolve_qualified(qualifier, name)?;
            let ty = scope.column_at(idx).ty;
            Ok((RExpr::Column(idx), resolved_type_of(ty)))
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
            // A string literal is text by default (collation `C`). It adapts to a BYTEA
            // context only (types.md §6/§13): decode the hex input form there (22P02 on
            // malformed hex). Any other context — including none — keeps it text.
            if matches!(ctx, Some(t) if t.is_bytea()) {
                Ok((
                    RExpr::ConstBytea(decode_bytea_literal(s)?),
                    ResolvedType::Bytea,
                ))
            } else {
                Ok((RExpr::ConstText(s.clone()), ResolvedType::Text))
            }
        }
        Expr::Literal(Literal::Decimal(d)) => {
            // A decimal literal is always decimal; it does not adapt to context (like text).
            // Cap-check it here (an over-long coefficient/scale traps 22003 at resolve —
            // spec/design/decimal.md §6).
            let d = d.clone().check_cap()?;
            Ok((RExpr::ConstDecimal(d), ResolvedType::Decimal))
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
            // The inner value is range-checked / coerced against `target` at eval, so it
            // resolves with no literal context here.
            let (rinner, ity) = resolve(scope, inner, None)?;
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
            let (rop, ty) = resolve(scope, operand, ctx)?;
            let result = match ty {
                ResolvedType::Int(t) => t,
                ResolvedType::Decimal => ScalarType::Decimal,
                ResolvedType::Null => ScalarType::Int64, // -NULL = NULL
                ResolvedType::Bool | ResolvedType::Text | ResolvedType::Bytea => {
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
            let (rop, ty) = resolve(scope, operand, None)?;
            require_bool(ty, "NOT requires a boolean operand")?;
            Ok((RExpr::Not(Box::new(rop)), ResolvedType::Bool))
        }
        Expr::IsNull { operand, negated } => {
            // IS [NOT] NULL accepts any operand type and always yields a definite boolean.
            let (rop, _ty) = resolve(scope, operand, None)?;
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
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs)?;
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
        Expr::Binary { op, lhs, rhs } => resolve_binary(scope, *op, lhs, rhs),
    }
}

fn resolve_binary(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
) -> Result<(RExpr, ResolvedType)> {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            // Arithmetic is overloaded across integer and decimal. Resolve the operand pair
            // (an integer literal adapts to an integer sibling), then pick the family: both
            // integer → integer arithmetic (promotion tower); at least one decimal → decimal
            // arithmetic (the integer operand widens at eval); a text/boolean operand is a
            // 42804 (spec/design/decimal.md §4).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs)?;
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
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs)?;
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
            let (rl, lt) = resolve(scope, lhs, None)?;
            let (rr, rt) = resolve(scope, rhs, None)?;
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
) -> Result<(RExpr, ResolvedType, RExpr, ResolvedType)> {
    let lhs_lit = is_adaptable_literal(lhs);
    let rhs_lit = is_adaptable_literal(rhs);
    let (rl, lt, rr, rt) = if lhs_lit && rhs_lit {
        // Two bare literals: no column context. Default an integer literal to int64; a string
        // literal stays text (no bytea context to decode it — types.md §6).
        let (rl, lt) = resolve(scope, lhs, Some(ScalarType::Int64))?;
        let (rr, rt) = resolve(scope, rhs, Some(ScalarType::Int64))?;
        (rl, lt, rr, rt)
    } else if lhs_lit {
        let (rr, rt) = resolve(scope, rhs, None)?;
        let (rl, lt) = resolve(scope, lhs, ctx_of(rt))?;
        (rl, lt, rr, rt)
    } else if rhs_lit {
        let (rl, lt) = resolve(scope, lhs, None)?;
        let (rr, rt) = resolve(scope, rhs, ctx_of(lt))?;
        (rl, lt, rr, rt)
    } else {
        let (rl, lt) = resolve(scope, lhs, None)?;
        let (rr, rt) = resolve(scope, rhs, None)?;
        (rl, lt, rr, rt)
    };
    Ok((rl, lt, rr, rt))
}

/// Whether `e` is a literal that adapts to its sibling operand's type (an integer or string
/// literal). NULL, boolean, and decimal literals do not take a sibling's context here.
fn is_adaptable_literal(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Literal(Literal::Int(_)) | Expr::Literal(Literal::Text(_))
    )
}

/// The context type a sibling operand offers an adaptable literal: an integer type (so an
/// integer literal adopts that width), or `bytea`/`text` (so a string literal can decode to
/// bytea, else stay text). `None` for bool/decimal/NULL — no useful literal context.
fn ctx_of(ty: ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(t),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool | ResolvedType::Decimal | ResolvedType::Null => None,
    }
}

/// Require that an arithmetic operand is numeric (integer or decimal, or NULL); a boolean,
/// text, or bytea operand is a 42804 type error.
fn require_numeric_operand(ty: ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null => Ok(()),
        ResolvedType::Bool | ResolvedType::Text | ResolvedType::Bytea => {
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
    use ResolvedType::{Bool, Bytea, Decimal, Int, Text};
    match (lt, rt) {
        // Boolean compares only with boolean (or NULL); boolean with a number/text/bytea is a mismatch.
        (Bool, Int(_))
        | (Int(_), Bool)
        | (Bool, Text)
        | (Text, Bool)
        | (Bool, Decimal)
        | (Decimal, Bool)
        | (Bool, Bytea)
        | (Bytea, Bool) => Err(type_error(
            "cannot compare a boolean value with a non-boolean value",
        )),
        (Int(_), Text) | (Text, Int(_)) | (Decimal, Text) | (Text, Decimal) => Err(type_error(
            "cannot compare a text value with a numeric value",
        )),
        // bytea compares only with bytea (or NULL); bytea with a number or text is a mismatch.
        (Bytea, Int(_))
        | (Int(_), Bytea)
        | (Bytea, Decimal)
        | (Decimal, Bytea)
        | (Bytea, Text)
        | (Text, Bytea) => Err(type_error(
            "cannot compare a bytea value with a non-bytea value",
        )),
        // Same-family pairs (numeric/numeric incl. int↔decimal, text/text, bool/bool,
        // bytea/bytea) and any pairing with a bare NULL literal are comparable.
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

fn require_bool(ty: ResolvedType, msg: &str) -> Result<()> {
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(()),
        ResolvedType::Int(_) | ResolvedType::Text | ResolvedType::Decimal | ResolvedType::Bytea => {
            Err(type_error(msg))
        }
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
    fn eval(&self, row: &[Value], m: &mut Meter) -> Result<Value> {
        match self {
            // The value is read out of a borrowed stored row, so it is cloned (Value is
            // Clone, not Copy, now that a text value owns a String).
            RExpr::Column(i) => Ok(row[*i].clone()),
            RExpr::ConstInt(n) => Ok(Value::Int(*n)),
            RExpr::ConstBool(b) => Ok(Value::Bool(*b)),
            RExpr::ConstText(s) => Ok(Value::Text(s.clone())),
            RExpr::ConstDecimal(d) => Ok(Value::Decimal(d.clone())),
            RExpr::ConstBytea(b) => Ok(Value::Bytea(b.clone())),
            RExpr::ConstNull => Ok(Value::Null),
            RExpr::Cast {
                inner,
                target,
                typmod,
            } => {
                m.charge(COSTS.operator_eval);
                match inner.eval(row, m)? {
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
                }
            }
            RExpr::Neg { operand, result } => {
                m.charge(COSTS.operator_eval);
                match operand.eval(row, m)? {
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
                }
            }
            RExpr::Not(e) => {
                m.charge(COSTS.operator_eval);
                let v = e.eval(row, m)?;
                Ok(not3(&v))
            }
            RExpr::Arith {
                op,
                lhs,
                rhs,
                result,
            } => {
                m.charge(COSTS.operator_eval);
                let a = lhs.eval(row, m)?;
                let b = rhs.eval(row, m)?;
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
                let a = lhs.eval(row, m)?;
                let b = rhs.eval(row, m)?;
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
                let lv = l.eval(row, m)?;
                let rv = r.eval(row, m)?;
                Ok(and3(&lv, &rv))
            }
            RExpr::Or(l, r) => {
                m.charge(COSTS.operator_eval);
                let lv = l.eval(row, m)?;
                let rv = r.eval(row, m)?;
                Ok(or3(&lv, &rv))
            }
            RExpr::IsNull { operand, negated } => {
                m.charge(COSTS.operator_eval);
                let is_null = matches!(operand.eval(row, m)?, Value::Null);
                // IS [NOT] NULL is always a definite boolean, never unknown (CLAUDE.md §4).
                Ok(Value::Bool(is_null != *negated))
            }
            RExpr::Distinct { lhs, rhs, negated } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, m)?;
                let rv = rhs.eval(row, m)?;
                let same = lv.not_distinct_from(&rv);
                // `negated` carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks
                // "are they the same?" → `same`; IS DISTINCT FROM asks the opposite. Either
                // way the result is a definite boolean — never unknown (the null_safe
                // discipline, functions.md §3).
                Ok(Value::Bool(same == *negated))
            }
        }
    }
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
    }
}
