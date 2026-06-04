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

// PrimaryKeyIndex returns the primary-key column's index, or -1. Step-1 supports at
// most a single-column primary key.
func (t *Table) PrimaryKeyIndex() int {
	for i, c := range t.Columns {
		if c.PrimaryKey {
			return i
		}
	}
	return -1
}
