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
	// Default is the column's DEFAULT value, pre-evaluated and type-coerced at CREATE TABLE, or
	// nil if it has no default. A non-nil pointer to a ValNull value is an explicit DEFAULT NULL.
	// Applied for an omitted column or a DEFAULT keyword at INSERT (spec/design/constraints.md §2).
	Default *Value
}

// Table is a table definition.
type Table struct {
	Name    string
	Columns []Column
	// Checks is the table's CHECK constraints in EVALUATION ORDER — ascending byte order
	// of the lowercased name (spec/design/constraints.md §4.4); the on-disk catalog stores
	// them in this same order. Empty for an unchecked table.
	Checks []CheckConstraint
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

// PKIndices returns the primary-key member columns' indices in KEY order. Key order is
// the flagged columns in declaration order — CREATE TABLE requires the constraint's list
// order to match (the documented 0A000 narrowing, spec/design/constraints.md §3), so the
// flag bits alone reconstruct the key. Empty = the table has no primary key (synthetic
// rowid keys).
func (t *Table) PKIndices() []int {
	var idxs []int
	for i, c := range t.Columns {
		if c.PrimaryKey {
			idxs = append(idxs, i)
		}
	}
	return idxs
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
