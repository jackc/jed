package abide

import "strings"

// Column is a column definition: name, declared type, nullability, primary-key flag.
type Column struct {
	Name       string
	Type       ScalarType
	PrimaryKey bool
	// NotNull is implied true for a PRIMARY KEY column.
	NotNull bool
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
