package abide

// SupportedCapabilities lists the capabilities this implementation currently
// supports (spec/conformance: the gating axis). The harness runs a corpus file iff
// every capability in the file's `# requires:` header is in this set. GROWS as
// Phases B–E land. A whole corpus file only runs once all its required capabilities
// are present, so the harness stays all-skip until the `core` profile is complete
// (Phase E); per-phase correctness is driven by the Go unit tests until then.
var SupportedCapabilities = []string{
	// Phase B — CREATE TABLE with typed columns + single-column PRIMARY KEY.
	"ddl.create_table",
	"ddl.primary_key",
}

// Execute parses and executes one SQL statement against db.
func Execute(db *Database, sql string) (Outcome, error) {
	stmt, err := ParseSQL(sql)
	if err != nil {
		return Outcome{}, err
	}
	return db.ExecuteStmt(stmt)
}
