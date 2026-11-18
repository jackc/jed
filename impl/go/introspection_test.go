package jed

// White-box catalog access stays in the test build. Applications use SQL catalogs.

// table inspects the latest committed table definition for white-box tests.
func (db *Database) table(name string) (*catTable, bool) { return db.committedEngine().table(name) }

// compositeType is the definition of composite type name in the latest committed snapshot, or nil.
func (db *Database) compositeType(name string) *compositeType {
	return db.committedEngine().compositeType(name)
}

// rowsInKeyOrder is a white-box test helper (CLAUDE.md §10): all rows of persistent table name in
// primary-key order from the latest committed snapshot. Not the embedding API.
func (db *Database) rowsInKeyOrder(name string) []storedRow {
	return db.committedEngine().rowsInKeyOrder(name)
}

// table is the definition of table name (exact name) as this session sees it, or false.
func (s *Session) table(name string) (*catTable, bool) { return s.engine.table(name) }

// compositeType is the definition of composite type name as this session sees it, or nil.
func (s *Session) compositeType(name string) *compositeType { return s.engine.compositeType(name) }

// rowsInKeyOrder is a white-box test helper (CLAUDE.md §10): all rows of table name in primary-key
// order as this session sees them. Not the embedding API.
func (s *Session) rowsInKeyOrder(name string) []storedRow { return s.engine.rowsInKeyOrder(name) }
