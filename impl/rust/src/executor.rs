//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{
    CompareOp, CreateTable, Insert, Literal, Operand, Predicate, Select, SelectExpr, SelectItems,
    Statement,
};
use crate::catalog::{Column, Table};
use crate::encoding::encode_int;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::{Row, TableStore};
use crate::types::ScalarType;
use crate::value::{ThreeValued, Value};
use std::collections::HashMap;

/// The outcome of executing one statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A statement that produces no result set (CREATE, INSERT).
    Statement,
    /// A query result: column count plus rows in result order.
    Query {
        column_count: usize,
        rows: Vec<Vec<Value>>,
    },
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
            let ty = ScalarType::from_name(&def.type_name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedObject,
                    format!("type does not exist: {}", def.type_name),
                )
            })?;
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
        Ok(Outcome::Statement)
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
            };
            row.push(value);
        }

        // The storage key is the encoded primary key, or — for a table without one
        // — a synthetic insertion-order rowid (rows are append-only in step-1).
        let key = match table.primary_key_index() {
            Some(i) => {
                let pk_ty = table.columns[i].ty;
                match row[i] {
                    Value::Int(n) => encode_int(pk_ty, n),
                    // Unreachable: a PK column is NOT NULL, enforced above.
                    Value::Null => unreachable!("primary key column is NOT NULL"),
                }
            }
            None => {
                let store = self.store(&ins.table);
                encode_int(ScalarType::Int64, store.len() as i64)
            }
        };

        let store = self.store_mut(&ins.table);
        if !store.insert(key, row) {
            return Err(EngineError::new(
                SqlState::UniqueViolation,
                "duplicate key value violates primary key uniqueness",
            ));
        }
        Ok(Outcome::Statement)
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

        // Resolve projections to (column index | cast | literal) against the table.
        let projections = self.resolve_projections(table, &sel.items)?;
        let column_count = projections.len();

        // Resolve the optional predicate's column up front.
        let filter = match &sel.filter {
            Some(p) => Some(self.resolve_predicate(table, p)?),
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
        // logical key order), then filter.
        let mut rows: Vec<Row> = self
            .store(&sel.from)
            .iter_in_key_order()
            .filter(|row| match &filter {
                None => true,
                Some(f) => f.eval(row).is_true(),
            })
            .cloned()
            .collect();

        // ORDER BY: a stable sort by the key column's value. NULLs sort first in
        // ascending order (spec/design/encoding.md §4); descending reverses, NULLs
        // last.
        if let Some((idx, descending)) = order {
            rows.sort_by(|a, b| {
                let ord = null_first_cmp(a[idx], b[idx]);
                if descending { ord.reverse() } else { ord }
            });
        }

        // Project each surviving row.
        let mut out_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut out = Vec::with_capacity(column_count);
            for p in &projections {
                out.push(p.eval(row)?);
            }
            out_rows.push(out);
        }

        Ok(Outcome::Query {
            column_count,
            rows: out_rows,
        })
    }

    /// Resolve `SELECT` items against a table into evaluable projections.
    fn resolve_projections(&self, table: &Table, items: &SelectItems) -> Result<Vec<Projection>> {
        match items {
            SelectItems::All => Ok((0..table.columns.len()).map(Projection::Column).collect()),
            SelectItems::Items(exprs) => {
                exprs.iter().map(|e| self.resolve_expr(table, e)).collect()
            }
        }
    }

    fn resolve_expr(&self, table: &Table, expr: &SelectExpr) -> Result<Projection> {
        match expr {
            SelectExpr::Column(name) => {
                let idx = table.column_index(name).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column does not exist: {name}"),
                    )
                })?;
                Ok(Projection::Column(idx))
            }
            SelectExpr::Literal(Literal::Int(n)) => Ok(Projection::LiteralInt(*n)),
            SelectExpr::Literal(Literal::Null) => Ok(Projection::Null),
            SelectExpr::Cast { inner, type_name } => {
                let target = ScalarType::from_name(type_name).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedObject,
                        format!("type does not exist: {type_name}"),
                    )
                })?;
                Ok(Projection::Cast {
                    inner: Box::new(self.resolve_expr(table, inner)?),
                    target,
                })
            }
        }
    }

    /// Resolve a predicate's column references into indices + a comparison plan.
    fn resolve_predicate(&self, table: &Table, p: &Predicate) -> Result<ResolvedPredicate> {
        let resolve_col = |name: &str| -> Result<usize> {
            table.column_index(name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column does not exist: {name}"),
                )
            })
        };
        match p {
            Predicate::Compare { column, op, rhs } => {
                let idx = resolve_col(column)?;
                let rhs = match rhs {
                    Operand::Literal(Literal::Null) => RhsPlan::Const(Value::Null),
                    Operand::Literal(Literal::Int(n)) => RhsPlan::Const(Value::Int(*n)),
                    Operand::Column(name) => RhsPlan::Column(resolve_col(name)?),
                };
                Ok(ResolvedPredicate::Compare { idx, op: *op, rhs })
            }
            Predicate::IsNull { column, negated } => Ok(ResolvedPredicate::IsNull {
                idx: resolve_col(column)?,
                negated: *negated,
            }),
        }
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

/// A resolved projection: how to produce one output column from a stored row.
enum Projection {
    /// The value of the row's column at this index.
    Column(usize),
    /// A constant integer literal (e.g. `SELECT CAST(1000 AS int16)`).
    LiteralInt(i64),
    /// A constant NULL.
    Null,
    /// Cast the inner projection's value to `target`, trapping on overflow (22003).
    Cast {
        inner: Box<Projection>,
        target: ScalarType,
    },
}

impl Projection {
    fn eval(&self, row: &[Value]) -> Result<Value> {
        match self {
            Projection::Column(i) => Ok(row[*i]),
            Projection::LiteralInt(n) => Ok(Value::Int(*n)),
            Projection::Null => Ok(Value::Null),
            Projection::Cast { inner, target } => match inner.eval(row)? {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => {
                    if target.in_range(n) {
                        Ok(Value::Int(n))
                    } else {
                        Err(EngineError::new(
                            SqlState::NumericValueOutOfRange,
                            format!("value out of range for type {}", target.canonical_name()),
                        ))
                    }
                }
            },
        }
    }
}

/// The right-hand side of a resolved comparison: a constant, or another column.
enum RhsPlan {
    Const(Value),
    Column(usize),
}

/// A resolved WHERE predicate over fixed column indices.
enum ResolvedPredicate {
    Compare {
        idx: usize,
        op: CompareOp,
        rhs: RhsPlan,
    },
    IsNull {
        idx: usize,
        negated: bool,
    },
}

impl ResolvedPredicate {
    /// Evaluate against a row, returning a three-valued result. A WHERE clause keeps
    /// a row only when this is TRUE (CLAUDE.md §4).
    fn eval(&self, row: &[Value]) -> ThreeValued {
        match self {
            ResolvedPredicate::Compare { idx, op, rhs } => {
                let lhs = row[*idx];
                let rhs = match rhs {
                    RhsPlan::Const(v) => *v,
                    RhsPlan::Column(j) => row[*j],
                };
                match op {
                    CompareOp::Eq => lhs.eq3(rhs),
                    CompareOp::Lt => lhs.lt3(rhs),
                    CompareOp::Gt => lhs.gt3(rhs),
                    CompareOp::Le => lhs.lt3(rhs).or(lhs.eq3(rhs)),
                    CompareOp::Ge => lhs.gt3(rhs).or(lhs.eq3(rhs)),
                }
            }
            ResolvedPredicate::IsNull { idx, negated } => {
                let is_null = matches!(row[*idx], Value::Null);
                let result = is_null != *negated;
                // IS [NOT] NULL is always TRUE or FALSE, never UNKNOWN (CLAUDE.md §4).
                if result {
                    ThreeValued::True
                } else {
                    ThreeValued::False
                }
            }
        }
    }
}

/// Total order over values for ORDER BY with NULLs sorting first (ascending),
/// matching the key encoding's physical order (spec/design/encoding.md §4).
fn null_first_cmp(a: Value, b: Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(&y),
    }
}
