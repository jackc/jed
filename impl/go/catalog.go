package jed

import "strings"

// Column is a column definition: name, declared type, nullability, primary-key flag.
type Column struct {
	Name string
	Type ScalarType
	// Decimal is the numeric(p,s) typmod for a decimal column, or nil for a non-decimal column
	// OR an unconstrained numeric (spec/design/decimal.md §2). A constrained decimal column
	// coerces stored values to this precision/scale.
	Decimal    *DecimalTypmod
	PrimaryKey bool
	// NotNull is implied true for a PRIMARY KEY column.
	NotNull bool
	// Default is the column's CONSTANT DEFAULT value, pre-evaluated and type-coerced at CREATE
	// TABLE, or nil if it has no default or an EXPRESSION default (DefaultExpr). A non-nil
	// pointer to a ValNull value is an explicit DEFAULT NULL. Applied for an omitted column or a
	// DEFAULT keyword at INSERT (spec/design/constraints.md §2).
	Default *Value
	// DefaultExpr is the column's EXPRESSION DEFAULT (a non-constant default like uuidv7() or
	// 1 + 1), or nil if it has no default or a constant default (Default). Mutually exclusive
	// with Default. Stored as expression text (re-rendered verbatim at every commit, like a
	// CHECK — spec/fileformat/format.md) plus the parsed expression the write paths resolve and
	// evaluate per row (spec/design/constraints.md §2).
	DefaultExpr *DefaultExpr
}

// DefaultExpr is a column's EXPRESSION DEFAULT (spec/design/constraints.md §2): its persisted
// expression text — written back verbatim at every commit so the catalog bytes are stable
// (spec/fileformat/format.md "Check-expression text") — and the parsed expression the write
// paths resolve (against an empty scope, no columns) and evaluate per inserted row. Modeled on
// CheckConstraint.
type DefaultExpr struct {
	ExprText string
	Expr     Expr
}

// Table is a table definition.
type Table struct {
	Name    string
	Columns []Column
	// PK is the primary-key member column ordinals in KEY order (which may differ from
	// declaration order — constraints.md §3; the v5 catalog persists this list). Empty =
	// no primary key (synthetic rowid keys). The per-column PrimaryKey flag is derived
	// membership convenience; this list is the authority for order.
	PK []int
	// Checks is the table's CHECK constraints in EVALUATION ORDER — ascending byte order
	// of the lowercased name (spec/design/constraints.md §4.4); the on-disk catalog stores
	// them in this same order. Empty for an unchecked table.
	Checks []CheckConstraint
	// Indexes is the table's secondary indexes in ascending lowercased-name order (the
	// catalog's on-disk order and the planner's tie-break order — spec/design/indexes.md).
	Indexes []IndexDef
}

// IndexDef is one secondary index of a table (spec/design/indexes.md): its
// (relation-namespace) name and the indexed column ordinals in index-key order
// (duplicates allowed — PG). The index's B-tree lives in the snapshot's index-store map,
// keyed by the lowercased name. A Unique index enforces uniqueness over its key tuple
// (NULLS DISTINCT — spec/design/indexes.md §8); it is what backs a UNIQUE constraint
// (spec/design/constraints.md §5).
type IndexDef struct {
	Name    string
	Columns []int
	Unique  bool
}

// CheckConstraint is one CHECK constraint: its (resolved, unique-per-table) name, its
// persisted expression text — written back verbatim at every commit so the catalog bytes
// are stable (spec/fileformat/format.md "Check-expression text") — and the parsed
// expression the write paths resolve and evaluate per candidate row (constraints.md §4).
type CheckConstraint struct {
	Name     string
	ExprText string
	Expr     Expr
}

// ColumnIndex returns the index of the named column (case-insensitive), or -1.
func (t *Table) ColumnIndex(name string) int {
	for i, c := range t.Columns {
		if strings.EqualFold(c.Name, name) {
			return i
		}
	}
	return -1
}

// PKIndices returns the primary-key member columns' indices in KEY order (the explicit
// PK list — the v5 catalog persists key order independent of declaration order). Empty =
// the table has no primary key (synthetic rowid keys).
func (t *Table) PKIndices() []int {
	return t.PK
}

// PrimaryKeyIndex returns the primary-key column's index iff the key is SINGLE-column,
// else -1. The PK pushdown (point lookup / range bound) recognizes single-column keys
// only — a composite-PK table full-scans this slice (spec/design/constraints.md §3) — so
// every pushdown site routes through this accessor and stays sound by construction.
func (t *Table) PrimaryKeyIndex() int {
	idxs := t.PKIndices()
	if len(idxs) == 1 {
		return idxs[0]
	}
	return -1
}
