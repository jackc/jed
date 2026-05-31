package abide

import (
	"fmt"
	"strings"
)

// Statement executor (CLAUDE.md §10).
//
// SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
// feature-by-feature (Phases B–E).

// OutcomeKind distinguishes a bare statement result from a query result set.
type OutcomeKind int

const (
	// OutcomeStatement is a statement producing no result set (CREATE, INSERT).
	OutcomeStatement OutcomeKind = iota
	// OutcomeQuery is a query result set.
	OutcomeQuery
)

// Outcome is the result of executing one statement.
type Outcome struct {
	Kind        OutcomeKind
	ColumnCount int
	Rows        [][]Value
}

// Database is the whole database: catalog + per-table in-memory stores. Single
// committed state (CLAUDE.md §3); the staging-buffer commit model lands with
// persistence.
type Database struct {
	tables map[string]*Table
	stores map[string]*TableStore
}

// NewDatabase builds an empty database.
func NewDatabase() *Database {
	return &Database{
		tables: make(map[string]*Table),
		stores: make(map[string]*TableStore),
	}
}

// Table looks up a table definition by name (case-insensitive).
func (db *Database) Table(name string) (*Table, bool) {
	t, ok := db.tables[strings.ToLower(name)]
	return t, ok
}

// putTable registers a new table and its empty store.
func (db *Database) putTable(t *Table) {
	key := strings.ToLower(t.Name)
	db.stores[key] = NewTableStore()
	db.tables[key] = t
}

// ExecuteStmt executes one parsed statement.
func (db *Database) ExecuteStmt(stmt Statement) (Outcome, error) {
	switch {
	case stmt.CreateTable != nil:
		return db.executeCreateTable(stmt.CreateTable)
	case stmt.Insert != nil:
		return db.executeInsert(stmt.Insert)
	case stmt.Select != nil:
		return Outcome{}, NewError(FeatureNotSupported,
			"statement execution is not implemented yet (step-5 Phase A scaffold)")
	default:
		return Outcome{}, NewError(SyntaxError, "empty statement")
	}
}

// executeCreateTable analyzes and runs a CREATE TABLE: resolve each column's type
// name, enforce a single primary key (which is implicitly NOT NULL), reject
// duplicate table and column names, then register the table.
func (db *Database) executeCreateTable(ct *CreateTable) (Outcome, error) {
	if _, ok := db.Table(ct.Name); ok {
		return Outcome{}, NewError(DuplicateTable, "table already exists: "+ct.Name)
	}

	columns := make([]Column, 0, len(ct.Columns))
	pkSeen := false
	for _, def := range ct.Columns {
		for _, c := range columns {
			if strings.EqualFold(c.Name, def.Name) {
				return Outcome{}, NewError(DuplicateColumn, "duplicate column name: "+def.Name)
			}
		}
		ty, ok := ScalarTypeFromName(def.TypeName)
		if !ok {
			return Outcome{}, NewError(UndefinedObject, "type does not exist: "+def.TypeName)
		}
		if def.PrimaryKey {
			if pkSeen {
				return Outcome{}, NewError(InvalidTableDefinition,
					"a table may have at most one primary key")
			}
			pkSeen = true
		}
		columns = append(columns, Column{
			Name:       def.Name,
			Type:       ty,
			PrimaryKey: def.PrimaryKey,
			NotNull:    def.PrimaryKey, // PRIMARY KEY ⇒ NOT NULL
		})
	}

	db.putTable(&Table{Name: ct.Name, Columns: columns})
	return Outcome{Kind: OutcomeStatement}, nil
}

// executeInsert analyzes and runs an INSERT: map the literal values positionally to
// columns, type-check each (NULL into NOT NULL traps 23502; an integer outside the
// column type's range traps 22003 — CLAUDE.md §8), then store the row keyed by its
// encoded primary key (duplicate key traps 23505).
func (db *Database) executeInsert(ins *Insert) (Outcome, error) {
	table, ok := db.Table(ins.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	if len(ins.Values) != len(table.Columns) {
		return Outcome{}, NewError(SyntaxError, fmt.Sprintf(
			"INSERT has %d values but table %s has %d columns",
			len(ins.Values), table.Name, len(table.Columns),
		))
	}

	row := make(Row, len(table.Columns))
	for i, col := range table.Columns {
		lit := ins.Values[i]
		switch lit.Kind {
		case LiteralNull:
			if col.NotNull {
				return Outcome{}, NewError(NotNullViolation,
					"null value in column "+col.Name+" violates not-null constraint")
			}
			row[i] = NullValue()
		case LiteralInt:
			if !col.Type.InRange(lit.Int) {
				return Outcome{}, NewError(NumericValueOutOfRange,
					"value out of range for type "+col.Type.CanonicalName())
			}
			row[i] = IntValue(lit.Int)
		}
	}

	// The storage key is the encoded primary key, or — for a table without one — a
	// synthetic insertion-order rowid (rows are append-only in step-1).
	store := db.stores[strings.ToLower(ins.Table)]
	var key []byte
	if pk := table.PrimaryKeyIndex(); pk >= 0 {
		key = EncodeInt(table.Columns[pk].Type, row[pk].Int)
	} else {
		key = EncodeInt(Int64, int64(store.Len()))
	}

	if !store.Insert(key, row) {
		return Outcome{}, NewError(UniqueViolation,
			"duplicate key value violates primary key uniqueness")
	}
	return Outcome{Kind: OutcomeStatement}, nil
}

// RowsInKeyOrder returns a table's rows in primary-key (encoded byte) order, or nil
// if the table does not exist. Used by SELECT and by tests.
func (db *Database) RowsInKeyOrder(name string) []Row {
	store, ok := db.stores[strings.ToLower(name)]
	if !ok {
		return nil
	}
	return store.IterInKeyOrder()
}
