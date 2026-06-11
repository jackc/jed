//! The catalog: table and column definitions (CLAUDE.md §4 strict static types).

use crate::ast::Expr;
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

/// One `CHECK` constraint: its (resolved, unique-per-table) name, its persisted expression
/// text — written back verbatim at every commit so the catalog bytes are stable
/// (spec/fileformat/format.md "Check-expression text") — and the parsed expression the
/// write paths resolve and evaluate per candidate row (spec/design/constraints.md §4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckConstraint {
    pub name: String,
    pub expr_text: String,
    pub expr: Expr,
}

/// One secondary index of a table (spec/design/indexes.md): its (relation-namespace) name
/// and the indexed column ordinals in index-key order (duplicates allowed — PG). The index's
/// B-tree lives in the snapshot's index-store map, keyed by the lowercased name. A
/// `unique` index enforces uniqueness over its key tuple (NULLS DISTINCT —
/// spec/design/indexes.md §8); it is what backs a `UNIQUE` constraint
/// (spec/design/constraints.md §5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<usize>,
    pub unique: bool,
}

/// A table definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The primary-key member column ordinals in **key order** (which may differ from
    /// declaration order — constraints.md §3; the v5 catalog persists this list). Empty =
    /// no primary key (synthetic rowid keys). The per-column `primary_key` flag is derived
    /// membership convenience; this list is the authority for order.
    pub pk: Vec<usize>,
    /// The table's CHECK constraints in **evaluation order** — ascending byte order of the
    /// lowercased name (spec/design/constraints.md §4.4); the on-disk catalog stores them in
    /// this same order. Empty for an unchecked table.
    pub checks: Vec<CheckConstraint>,
    /// The table's secondary indexes in **ascending lowercased-name order** (the catalog's
    /// on-disk order and the planner's tie-break order — spec/design/indexes.md §5/§6).
    pub indexes: Vec<IndexDef>,
}

impl Table {
    /// Index of the named column (case-insensitive), if present.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The primary-key member columns' indices in KEY order (the explicit `pk` list — the
    /// v5 catalog persists key order independent of declaration order). Empty = the table
    /// has no primary key (synthetic rowid keys).
    pub fn pk_indices(&self) -> Vec<usize> {
        self.pk.clone()
    }

    /// The primary-key column's index iff the key is SINGLE-column. The PK pushdown
    /// (point lookup / range bound) recognizes single-column keys only — a composite-PK
    /// table full-scans this slice (spec/design/constraints.md §3) — so every pushdown
    /// site routes through this accessor and stays sound by construction.
    pub fn primary_key_index(&self) -> Option<usize> {
        match self.pk_indices().as_slice() {
            [i] => Some(*i),
            _ => None,
        }
    }
}
