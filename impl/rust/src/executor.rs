//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{
    BinaryOp, CreateTable, Delete, Expr, Insert, Literal, Select, SelectItems, Statement, UnaryOp,
    Update,
};
use crate::catalog::{Column, Table};
use crate::cost::Meter;
use crate::costs::COSTS;
use crate::encoding::encode_int;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::{Row, TableStore};
use crate::types::{ScalarType, is_boolean_type_name};
use crate::value::{Value, and3, from3, not3, or3};
use std::collections::HashMap;

/// The outcome of executing one statement. Both variants carry the deterministic
/// execution `cost` accrued while running the statement (CLAUDE.md §13) — a DML
/// statement accrues its scan + filter cost even though it returns no rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A statement that produces no result set (CREATE, INSERT, UPDATE, DELETE).
    Statement { cost: i64 },
    /// A query result: column count plus rows in result order.
    Query {
        column_count: usize,
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
            let ty = resolve_storable_type(&def.type_name)?;
            if def.primary_key {
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

    /// Analyze and run an INSERT: map the literal values positionally to columns,
    /// type-check each (NULL into NOT NULL traps 23502; an integer outside the
    /// column type's range traps 22003 — CLAUDE.md §8), then store the row keyed by
    /// its encoded primary key (duplicate key traps 23505).
    fn execute_insert(&mut self, ins: Insert) -> Result<Outcome> {
        let table = self.table(&ins.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", ins.table),
            )
        })?;

        if ins.values.len() != table.columns.len() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                format!(
                    "INSERT has {} values but table {} has {} columns",
                    ins.values.len(),
                    table.name,
                    table.columns.len()
                ),
            ));
        }

        // Coerce + type-check each value against its column.
        let mut row = Vec::with_capacity(table.columns.len());
        for (col, lit) in table.columns.iter().zip(&ins.values) {
            let value = match lit {
                Literal::Null => {
                    if col.not_null {
                        return Err(EngineError::new(
                            SqlState::NotNullViolation,
                            format!(
                                "null value in column {} violates not-null constraint",
                                col.name
                            ),
                        ));
                    }
                    Value::Null
                }
                Literal::Int(n) => {
                    if !col.ty.in_range(*n) {
                        return Err(EngineError::new(
                            SqlState::NumericValueOutOfRange,
                            format!("value out of range for type {}", col.ty.canonical_name()),
                        ));
                    }
                    Value::Int(*n)
                }
                // boolean is expression-only: there are no boolean columns, so a boolean
                // literal can only target an integer column — a type error (42804).
                Literal::Bool(_) => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!(
                            "cannot store a boolean value in integer column {}",
                            col.name
                        ),
                    ));
                }
            };
            row.push(value);
        }

        // The storage key is the encoded primary key, or — for a table without one —
        // a monotonic synthetic rowid. Compute the PK column up front so the table
        // borrow ends before the no-PK arm needs `store_mut`.
        let pk = table.primary_key_index().map(|i| (i, table.columns[i].ty));
        let key = match pk {
            Some((i, pk_ty)) => match row[i] {
                Value::Int(n) => encode_int(pk_ty, n),
                // Unreachable: a PK column is NOT NULL, enforced above.
                Value::Null => unreachable!("primary key column is NOT NULL"),
                // Unreachable: boolean is expression-only; no column is boolean.
                Value::Bool(_) => unreachable!("a boolean cannot be a stored column value"),
            },
            // Monotonic rowid: never reused, so DELETE then INSERT cannot collide
            // with a freed key (spec/fileformat/format.md).
            None => encode_int(ScalarType::Int64, self.store_mut(&ins.table).alloc_rowid()),
        };

        let store = self.store_mut(&ins.table);
        if !store.insert(key, row) {
            return Err(EngineError::new(
                SqlState::UniqueViolation,
                "duplicate key value violates primary key uniqueness",
            ));
        }
        // A single-row INSERT of literal values reads no rows and evaluates no
        // expression tree: zero cost (DEFAULT expressions, when added, will accrue here).
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
        let filter = match &del.filter {
            Some(p) => Some(resolve_boolean_filter(table, p)?),
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
            // operand adapts to the target column's type. The result must be an integer
            // (or NULL) — assigning a boolean to an integer column is a 42804.
            let (source, ty) = resolve(table, &a.value, Some(col.ty))?;
            require_assignable_int(ty, &a.column)?;
            plans.push(AssignPlan {
                idx,
                name: col.name.clone(),
                target: col.ty,
                not_null: col.not_null,
                source,
            });
        }

        let filter = match &upd.filter {
            Some(p) => Some(resolve_boolean_filter(table, p)?),
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
        let table = self.table(&sel.from).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", sel.from),
            )
        })?;

        // Resolve projections to evaluable expression trees against the table.
        let projections = resolve_projections(table, &sel.items)?;
        let column_count = projections.len();

        // Resolve the optional WHERE expression; it must resolve to boolean.
        let filter = match &sel.filter {
            Some(p) => Some(resolve_boolean_filter(table, p)?),
            None => None,
        };

        // Resolve the optional ORDER BY column.
        let order = match &sel.order_by {
            Some(ob) => {
                let idx = table.column_index(&ob.column).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column does not exist: {}", ob.column),
                    )
                })?;
                Some((idx, ob.descending))
            }
            None => None,
        };

        // Scan in primary-key order (the order-preserving encoding makes this the
        // logical key order), then filter. A WHERE arithmetic can trap (22003/22012),
        // so this is an explicit loop that propagates the error. Each scanned row and the
        // filter evaluation accrue cost; the row-produced charge is below, at projection
        // (CLAUDE.md §13; spec/design/cost.md §3).
        let mut meter = Meter::new();
        let mut rows: Vec<Row> = Vec::new();
        for row in self.store(&sel.from).iter_in_key_order() {
            meter.charge(COSTS.storage_row_read);
            let keep = match &filter {
                None => true,
                Some(f) => f.eval(row, &mut meter)?.is_true(),
            };
            if keep {
                rows.push(row.clone());
            }
        }

        // ORDER BY: a stable sort by the key column's value. NULLs sort first in
        // ascending order (spec/design/encoding.md §4); descending reverses, NULLs
        // last.
        if let Some((idx, descending)) = order {
            rows.sort_by(|a, b| {
                let ord = null_first_cmp(a[idx], b[idx]);
                if descending { ord.reverse() } else { ord }
            });
        }

        // Project each surviving row. Producing a row, and each projection-list
        // evaluation, accrue cost. (ORDER BY's sort comparisons are not metered —
        // spec/design/cost.md §3.)
        let mut out_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            meter.charge(COSTS.row_produced);
            let mut out = Vec::with_capacity(column_count);
            for p in &projections {
                out.push(p.eval(row, &mut meter)?);
            }
            out_rows.push(out);
        }

        Ok(Outcome::Query {
            column_count,
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
// Resolved expression layer.
//
// Parse → `Expr` (names) → resolve → `RExpr` (column indices, known result types,
// folded constants) → eval per row → `Value`. The resolver is where all
// type-checking and the literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

/// The static type of a resolved expression. `Null` is an untyped NULL literal (its
/// integer type, if needed, is settled by the surrounding operator/context).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedType {
    Int(ScalarType),
    Bool,
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
    ConstNull,
    Cast {
        inner: Box<RExpr>,
        target: ScalarType,
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

/// Resolve `SELECT` items against a table into evaluable projections (any result type
/// is allowed in the select list, including boolean — `SELECT a = b`).
fn resolve_projections(table: &Table, items: &SelectItems) -> Result<Vec<RExpr>> {
    match items {
        SelectItems::All => Ok((0..table.columns.len()).map(RExpr::Column).collect()),
        SelectItems::Items(exprs) => exprs
            .iter()
            .map(|e| resolve(table, e, None).map(|(node, _)| node))
            .collect(),
    }
}

/// Resolve a WHERE expression: it must resolve to boolean (or an untyped NULL, which
/// is always unknown → no rows). An integer-valued WHERE is a 42804 type error.
fn resolve_boolean_filter(table: &Table, e: &Expr) -> Result<RExpr> {
    let (node, ty) = resolve(table, e, None)?;
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(node),
        ResolvedType::Int(_) => Err(type_error("argument of WHERE must be boolean")),
    }
}

/// Resolve one `Expr` into an `RExpr` plus its static type. `ctx` is the type an
/// untyped integer literal should adapt to (spec/design/types.md §6); `None` defaults
/// a bare literal to int64.
fn resolve(table: &Table, e: &Expr, ctx: Option<ScalarType>) -> Result<(RExpr, ResolvedType)> {
    match e {
        Expr::Column(name) => {
            let idx = col_idx(table, name)?;
            Ok((RExpr::Column(idx), ResolvedType::Int(table.columns[idx].ty)))
        }
        Expr::Literal(Literal::Null) => Ok((RExpr::ConstNull, ResolvedType::Null)),
        Expr::Literal(Literal::Bool(b)) => Ok((RExpr::ConstBool(*b), ResolvedType::Bool)),
        Expr::Literal(Literal::Int(n)) => {
            let ty = ctx.unwrap_or(ScalarType::Int64);
            if !ty.in_range(*n) {
                return Err(overflow(ty));
            }
            Ok((RExpr::ConstInt(*n), ResolvedType::Int(ty)))
        }
        Expr::Cast { inner, type_name } => {
            let target = resolve_storable_type(type_name)?;
            // The inner value is range-checked against `target` at eval (its own
            // context), so it resolves with no literal context here.
            let (rinner, ity) = resolve(table, inner, None)?;
            match ity {
                ResolvedType::Int(_) | ResolvedType::Null => {}
                ResolvedType::Bool => {
                    return Err(type_error(format!(
                        "cannot cast boolean to {}",
                        target.canonical_name()
                    )));
                }
            }
            Ok((
                RExpr::Cast {
                    inner: Box::new(rinner),
                    target,
                },
                ResolvedType::Int(target),
            ))
        }
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => {
            let (rop, ty) = resolve(table, operand, ctx)?;
            let result = match ty {
                ResolvedType::Int(t) => t,
                ResolvedType::Null => ScalarType::Int64, // -NULL = NULL
                ResolvedType::Bool => {
                    return Err(type_error("unary minus requires an integer operand"));
                }
            };
            Ok((
                RExpr::Neg {
                    operand: Box::new(rop),
                    result,
                },
                ResolvedType::Int(result),
            ))
        }
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => {
            let (rop, ty) = resolve(table, operand, None)?;
            require_bool(ty, "NOT requires a boolean operand")?;
            Ok((RExpr::Not(Box::new(rop)), ResolvedType::Bool))
        }
        Expr::IsNull { operand, negated } => {
            // IS [NOT] NULL accepts any operand type and always yields a definite boolean.
            let (rop, _ty) = resolve(table, operand, None)?;
            Ok((
                RExpr::IsNull {
                    operand: Box::new(rop),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::IsDistinctFrom { lhs, rhs, negated } => {
            // NULL-safe equality: the SAME integer operand contract as `=` (promote a
            // mixed-width pair, adapt a literal to the sibling's type and range-check it).
            // The result is always a definite boolean (functions.md §3).
            let (rl, _lt, rr, _rt) = resolve_int_pair(table, lhs, rhs)?;
            Ok((
                RExpr::Distinct {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Binary { op, lhs, rhs } => resolve_binary(table, *op, lhs, rhs),
    }
}

fn resolve_binary(
    table: &Table,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
) -> Result<(RExpr, ResolvedType)> {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let (rl, lt, rr, rt) = resolve_int_pair(table, lhs, rhs)?;
            let result = promote(lt, rt);
            let aop = match op {
                BinaryOp::Add => ArithOp::Add,
                BinaryOp::Sub => ArithOp::Sub,
                BinaryOp::Mul => ArithOp::Mul,
                BinaryOp::Div => ArithOp::Div,
                BinaryOp::Mod => ArithOp::Mod,
                _ => unreachable!(),
            };
            Ok((
                RExpr::Arith {
                    op: aop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    result,
                },
                ResolvedType::Int(result),
            ))
        }
        BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            let (rl, _lt, rr, _rt) = resolve_int_pair(table, lhs, rhs)?;
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
            let (rl, lt) = resolve(table, lhs, None)?;
            let (rr, rt) = resolve(table, rhs, None)?;
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

/// Resolve the two operands of an arithmetic or comparison operator, giving each a
/// bare integer literal the *other* operand's type as context (so `small + 1` types
/// `1` as int16, and `small + 100000` traps 22003 at resolve). Both must be integer
/// (or NULL); a boolean operand is a 42804 type error.
fn resolve_int_pair(
    table: &Table,
    lhs: &Expr,
    rhs: &Expr,
) -> Result<(RExpr, ResolvedType, RExpr, ResolvedType)> {
    let lhs_lit = matches!(lhs, Expr::Literal(Literal::Int(_)));
    let rhs_lit = matches!(rhs, Expr::Literal(Literal::Int(_)));
    let (rl, lt, rr, rt) = if lhs_lit && rhs_lit {
        // Both bare literals: no column context, default to int64 (types.md §6).
        let (rl, lt) = resolve(table, lhs, Some(ScalarType::Int64))?;
        let (rr, rt) = resolve(table, rhs, Some(ScalarType::Int64))?;
        (rl, lt, rr, rt)
    } else if lhs_lit {
        let (rr, rt) = resolve(table, rhs, None)?;
        let (rl, lt) = resolve(table, lhs, int_type(rt))?;
        (rl, lt, rr, rt)
    } else if rhs_lit {
        let (rl, lt) = resolve(table, lhs, None)?;
        let (rr, rt) = resolve(table, rhs, int_type(lt))?;
        (rl, lt, rr, rt)
    } else {
        let (rl, lt) = resolve(table, lhs, None)?;
        let (rr, rt) = resolve(table, rhs, None)?;
        (rl, lt, rr, rt)
    };
    require_int_operand(lt)?;
    require_int_operand(rt)?;
    Ok((rl, lt, rr, rt))
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

fn require_int_operand(ty: ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Null => Ok(()),
        ResolvedType::Bool => Err(type_error(
            "arithmetic and comparison operators require integer operands",
        )),
    }
}

fn require_bool(ty: ResolvedType, msg: &str) -> Result<()> {
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(()),
        ResolvedType::Int(_) => Err(type_error(msg)),
    }
}

/// A value assigned to an integer column must itself be integer (or NULL); a boolean
/// expression is a 42804 type error.
fn require_assignable_int(ty: ResolvedType, col: &str) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Null => Ok(()),
        ResolvedType::Bool => Err(type_error(format!(
            "cannot assign a boolean value to integer column {col}"
        ))),
    }
}

fn col_idx(table: &Table, name: &str) -> Result<usize> {
    table.column_index(name).ok_or_else(|| {
        EngineError::new(
            SqlState::UndefinedColumn,
            format!("column does not exist: {name}"),
        )
    })
}

/// Resolve a type name used in a column definition or a CAST target. Only the storable
/// integer types are valid; `boolean` is a known-but-not-storable type this slice
/// (→ 0A000), distinct from a genuinely unknown name (→ 42704).
fn resolve_storable_type(name: &str) -> Result<ScalarType> {
    if let Some(ty) = ScalarType::from_name(name) {
        Ok(ty)
    } else if is_boolean_type_name(name) {
        Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("boolean is not a storable type yet: {name}"),
        ))
    } else {
        Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type does not exist: {name}"),
        ))
    }
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

/// A resolved UPDATE assignment: which column to write, the target type/nullability so
/// the new value is re-checked exactly like INSERT, and the resolved RHS expression
/// (evaluated against the *old* row).
struct AssignPlan {
    idx: usize,
    name: String,
    target: ScalarType,
    not_null: bool,
    source: RExpr,
}

impl AssignPlan {
    /// Type-check a candidate value against this column: NULL into NOT NULL traps
    /// 23502; an integer outside the target range traps 22003 (CLAUDE.md §8) — mirrors
    /// INSERT's per-value checks. The resolver already proved the value is integer or
    /// NULL (never boolean), so a boolean here is unreachable.
    fn check(&self, v: Value) -> Result<Value> {
        match v {
            Value::Null => {
                if self.not_null {
                    return Err(EngineError::new(
                        SqlState::NotNullViolation,
                        format!(
                            "null value in column {} violates not-null constraint",
                            self.name
                        ),
                    ));
                }
                Ok(Value::Null)
            }
            Value::Int(n) => {
                if self.target.in_range(n) {
                    Ok(Value::Int(n))
                } else {
                    Err(overflow(self.target))
                }
            }
            Value::Bool(_) => unreachable!("resolver rejects assigning a boolean to a column"),
        }
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
            RExpr::Column(i) => Ok(row[*i]),
            RExpr::ConstInt(n) => Ok(Value::Int(*n)),
            RExpr::ConstBool(b) => Ok(Value::Bool(*b)),
            RExpr::ConstNull => Ok(Value::Null),
            RExpr::Cast { inner, target } => {
                m.charge(COSTS.operator_eval);
                match inner.eval(row, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) => {
                        if target.in_range(n) {
                            Ok(Value::Int(n))
                        } else {
                            Err(overflow(*target))
                        }
                    }
                    Value::Bool(_) => unreachable!("resolver rejects a boolean cast operand"),
                }
            }
            RExpr::Neg { operand, result } => {
                m.charge(COSTS.operator_eval);
                match operand.eval(row, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) => {
                        // checked_neg guards i64::MIN; then range-check the result type.
                        let v = n.checked_neg().ok_or_else(|| overflow(*result))?;
                        if result.in_range(v) {
                            Ok(Value::Int(v))
                        } else {
                            Err(overflow(*result))
                        }
                    }
                    Value::Bool(_) => unreachable!("resolver rejects a boolean unary minus"),
                }
            }
            RExpr::Not(e) => {
                m.charge(COSTS.operator_eval);
                Ok(not3(e.eval(row, m)?))
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
                match (a, b) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::Int(x), Value::Int(y)) => eval_arith(*op, x, y, *result),
                    _ => unreachable!("resolver rejects boolean arithmetic operands"),
                }
            }
            RExpr::Compare { op, lhs, rhs } => {
                m.charge(COSTS.operator_eval);
                let a = lhs.eval(row, m)?;
                let b = rhs.eval(row, m)?;
                let tv = match op {
                    CmpOp::Eq => a.eq3(b),
                    CmpOp::Lt => a.lt3(b),
                    CmpOp::Gt => a.gt3(b),
                    CmpOp::Le => a.lt3(b).or(a.eq3(b)),
                    CmpOp::Ge => a.gt3(b).or(a.eq3(b)),
                };
                Ok(from3(tv))
            }
            RExpr::And(l, r) => {
                m.charge(COSTS.operator_eval);
                Ok(and3(l.eval(row, m)?, r.eval(row, m)?))
            }
            RExpr::Or(l, r) => {
                m.charge(COSTS.operator_eval);
                Ok(or3(l.eval(row, m)?, r.eval(row, m)?))
            }
            RExpr::IsNull { operand, negated } => {
                m.charge(COSTS.operator_eval);
                let is_null = matches!(operand.eval(row, m)?, Value::Null);
                // IS [NOT] NULL is always a definite boolean, never unknown (CLAUDE.md §4).
                Ok(Value::Bool(is_null != *negated))
            }
            RExpr::Distinct { lhs, rhs, negated } => {
                m.charge(COSTS.operator_eval);
                let same = lhs.eval(row, m)?.not_distinct_from(rhs.eval(row, m)?);
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

/// Total order over values for ORDER BY with NULLs sorting first (ascending),
/// matching the key encoding's physical order (spec/design/encoding.md §4). ORDER BY
/// is over a (always-integer) column this slice, so the boolean arms are not reached
/// from SELECT, but the order is defined (false < true) for totality.
fn null_first_cmp(a: Value, b: Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(&y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(&y),
        (Value::Bool(_), Value::Int(_)) => Ordering::Less,
        (Value::Int(_), Value::Bool(_)) => Ordering::Greater,
    }
}
