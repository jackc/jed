package abide

import "strings"

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
	case stmt.CreateTable != nil, stmt.Insert != nil, stmt.Select != nil:
		return Outcome{}, NewError(FeatureNotSupported,
			"statement execution is not implemented yet (step-5 Phase A scaffold)")
	default:
		return Outcome{}, NewError(SyntaxError, "empty statement")
	}
}
