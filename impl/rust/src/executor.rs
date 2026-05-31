//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{CreateTable, Insert, Literal, Statement};
use crate::catalog::{Column, Table};
use crate::encoding::encode_int;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::{Row, TableStore};
use crate::types::ScalarType;
use crate::value::Value;
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

    /// Execute one parsed statement.
    pub fn execute_stmt(&mut self, stmt: Statement) -> Result<Outcome> {
        match stmt {
            Statement::CreateTable(ct) => self.execute_create_table(ct),
            Statement::Insert(ins) => self.execute_insert(ins),
            Statement::Select(_) => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "statement execution is not implemented yet (step-5 Phase A scaffold)",
            )),
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

    /// Shared read access to a table's store (the table is known to exist).
    fn store(&self, name: &str) -> &TableStore {
        self.stores
            .get(&name.to_ascii_lowercase())
            .expect("store exists for a known table")
    }

    /// Mutable access to a table's store (the table is known to exist).
    fn store_mut(&mut self, name: &str) -> &mut TableStore {
        self.stores
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a known table")
    }
}
