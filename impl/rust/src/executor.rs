//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::Statement;
use crate::catalog::Table;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::TableStore;
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

    /// Register a new table and its (empty) store. Lower-cased name is the key.
    pub(crate) fn put_table(&mut self, table: Table) {
        let key = table.name.to_ascii_lowercase();
        self.stores.insert(key.clone(), TableStore::new());
        self.tables.insert(key, table);
    }

    /// Execute one parsed statement.
    pub fn execute_stmt(&mut self, stmt: Statement) -> Result<Outcome> {
        match stmt {
            Statement::CreateTable(_) | Statement::Insert(_) | Statement::Select(_) => {
                Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "statement execution is not implemented yet (step-5 Phase A scaffold)",
                ))
            }
        }
    }
}
