//! The catalog: table and column definitions (CLAUDE.md §4 strict static types).

use std::collections::HashMap;

use crate::ast::Expr;
use crate::types::{DecimalTypmod, ScalarType, Type};
use crate::value::Value;

/// A column definition: name, declared type, nullability, primary-key flag, default.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Column {
    pub name: String,
    /// The column's declared type — a built-in scalar or a user-defined composite
    /// (spec/design/composite.md). The open `Type` wrapper (CLAUDE.md §4): scalar-only call sites
    /// read `ty.scalar()`; the value codec / resolver branch on `Type::Composite`.
    pub ty: Type,
    /// The `numeric(p,s)` type modifier for a decimal column, or `None` for a non-decimal
    /// column OR an unconstrained `numeric` (spec/design/decimal.md §2). A constrained
    /// decimal column coerces stored values to this precision/scale.
    pub decimal: Option<DecimalTypmod>,
    pub primary_key: bool,
    /// A PRIMARY KEY column is implicitly NOT NULL.
    pub not_null: bool,
    /// The column's **constant** `DEFAULT` value, pre-evaluated and type-coerced at CREATE
    /// TABLE, or `None` if it has no default or an *expression* default (`default_expr`).
    /// `Some(Value::Null)` is an explicit `DEFAULT NULL`. Applied for an omitted column or a
    /// `DEFAULT` keyword at INSERT (spec/design/constraints.md §2).
    pub default: Option<Value>,
    /// The column's **expression** `DEFAULT` (a non-constant default like `uuidv7()` or
    /// `1 + 1`), or `None` if it has no default or a *constant* default (`default`). Mutually
    /// exclusive with `default`. Stored as expression text (re-rendered verbatim at every
    /// commit, like a `CHECK` — spec/fileformat/format.md) plus the parsed expression the write
    /// paths resolve and evaluate per row (spec/design/constraints.md §2).
    pub default_expr: Option<DefaultExpr>,
}

/// A column's **expression** `DEFAULT` (spec/design/constraints.md §2): its persisted
/// expression text — written back verbatim at every commit so the catalog bytes are stable
/// (spec/fileformat/format.md "Check-expression text") — and the parsed expression the write
/// paths resolve (against an empty scope, no columns) and evaluate per inserted row. Modeled on
/// `CheckConstraint`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefaultExpr {
    pub expr_text: String,
    pub expr: Expr,
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

/// A user-defined **composite (row) type** (spec/design/composite.md): a named, ordered list of
/// typed fields, living in the database's type catalog (a database-level object, not per-table).
/// Created by `CREATE TYPE name AS (field type, …)`, referenced by name from a column's `Type`.
/// Recursive — a field's `ty` may itself be `Type::Composite` (a nested composite, persisted
/// by name; spec/fileformat/format.md *Composite-type entry*).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompositeType {
    /// The type name (original case — round-trips what the user typed); looked up case-insensitively.
    pub name: String,
    /// The fields in declaration order (≥ 1).
    pub fields: Vec<CompositeField>,
}

/// One field of a composite type: its name, type, and declared nullability.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompositeField {
    pub name: String,
    pub ty: Type,
    /// The decimal `numeric(p,s)` typmod when `ty` is `decimal`, else `None` (mirrors `Column`).
    pub decimal: Option<DecimalTypmod>,
    /// Whether the field was declared `NOT NULL`.
    pub not_null: bool,
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

/// A fully-resolved storage/codec column type (spec/design/composite.md §4): a scalar, or a
/// composite resolved to the codec/coercion tree of its fields. Built **once** from a catalog
/// `Type` against the snapshot's composite-type definitions ([`resolve_col_type`]) and held by the
/// `TableStore`, so the value codec and store-coercion never re-walk the type catalog on every row.
/// Recursive — a composite field may itself be composite. The codec reads only the scalar / field
/// structure; the field `typmod` / `not_null` are consulted by store-coercion (executor).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ColType {
    Scalar(ScalarType),
    /// A composite type's resolved fields, in declaration order. `name` is the (original-case)
    /// type name, used in store-coercion error messages.
    Composite {
        name: String,
        fields: Vec<ColField>,
    },
}

/// One resolved field of a [`ColType::Composite`] — its name, recursively-resolved type, the
/// decimal typmod (when the field is `decimal`), and declared nullability (mirrors `CompositeField`,
/// but with the type fully resolved for the codec/coercion path).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColField {
    pub name: String,
    pub ty: ColType,
    pub typmod: Option<DecimalTypmod>,
    pub not_null: bool,
}

/// Resolve a catalog [`Type`] into a self-contained [`ColType`] against the database's composite
/// definitions (keyed by lowercased name, the `Snapshot.types` map). A composite reference is
/// looked up case-insensitively and recursively resolved; the lookup is guaranteed to succeed
/// because `validate_composite_types` (the two-pass load / `CREATE TYPE` gate) proved every
/// reference exists and the graph is acyclic before any store is built (spec/design/composite.md §3).
pub fn resolve_col_type(ty: &Type, types: &HashMap<String, CompositeType>) -> ColType {
    match ty {
        Type::Scalar(s) => ColType::Scalar(*s),
        Type::Composite(r) => {
            let def = types
                .get(&r.name.to_ascii_lowercase())
                .expect("composite type reference resolved by validate_composite_types");
            ColType::Composite {
                name: def.name.clone(),
                fields: def
                    .fields
                    .iter()
                    .map(|f| ColField {
                        name: f.name.clone(),
                        ty: resolve_col_type(&f.ty, types),
                        typmod: f.decimal,
                        not_null: f.not_null,
                    })
                    .collect(),
            }
        }
    }
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
