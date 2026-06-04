//! The catalog: table and column definitions (CLAUDE.md §4 strict static types).

use crate::types::{DecimalTypmod, ScalarType};
use crate::value::Value;

/// A column definition: name, declared type, nullability, primary-key flag, default.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ScalarType,
    /// The `numeric(p,s)` type modifier for a decimal column, or `None` for a non-decimal
    /// column OR an unconstrained `numeric` (spec/design/decimal.md §2). A constrained
    /// decimal column coerces stored values to this precision/scale.
    pub decimal: Option<DecimalTypmod>,
    pub primary_key: bool,
    /// A PRIMARY KEY column is implicitly NOT NULL.
    pub not_null: bool,
    /// The column's `DEFAULT` value, pre-evaluated and type-coerced at CREATE TABLE, or
    /// `None` if it has no default. `Some(Value::Null)` is an explicit `DEFAULT NULL`. Applied
    /// for an omitted column or a `DEFAULT` keyword at INSERT (spec/design/constraints.md §2).
    pub default: Option<Value>,
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
