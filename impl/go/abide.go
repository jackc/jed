package abide

// SupportedCapabilities lists the capabilities this implementation currently
// supports (spec/conformance: the gating axis). The harness runs a corpus file iff
// every capability in the file's `# requires:` header is in this set. GROWS as
// Phases B–E land; in the Phase A scaffold the engine supports no SQL features yet,
// so this is empty and zero conformance files run (the foundation tests still pass).
var SupportedCapabilities = []string{}

// Execute parses and executes one SQL statement against db.
func Execute(db *Database, sql string) (Outcome, error) {
	stmt, err := ParseSQL(sql)
	if err != nil {
		return Outcome{}, err
	}
	return db.ExecuteStmt(stmt)
}
