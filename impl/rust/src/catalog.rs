//! The catalog: table and column definitions (CLAUDE.md §4 strict static types).

use crate::types::ScalarType;

/// A column definition: name, declared type, nullability, primary-key flag.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ScalarType,
    pub primary_key: bool,
    /// A PRIMARY KEY column is implicitly NOT NULL.
    pub not_null: bool,
}

/// A table definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

impl Table {
    /// Index of the named column (case-insensitive), if present.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The primary-key column's index, if the table has one. Step-1 supports at
    /// most a single-column primary key.
    pub fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }
}
